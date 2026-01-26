use std::{mem, time::Instant};

    #[cfg(feature = "libcamera")]
    use crate::capture_api::libcamera_backend::{ControlMessage, PendingControlState};
use crate::metrics::StageMetrics;
use crate::{BackendKind, ProbedBackend};

#[cfg(feature = "v4l2")]
use super::controls::{apply_v4l2_controls, read_v4l2_control};
#[cfg(feature = "file-backend")]
use super::file_backend;
#[cfg(feature = "libcamera")]
use super::libcamera_backend;
#[cfg(feature = "netcam")]
use super::netcam_backend;
use super::request::{CaptureError, TdnOutputMode};
#[cfg(feature = "v4l2")]
use super::v4l2_backend;
use super::virtual_backend;
use styx_capture::prelude::*;

/// Control plane handle for applying backend-specific controls.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ControlPlane {
    None,
    #[cfg(feature = "v4l2")]
    V4l2 {
        path: String,
    },
    #[cfg(feature = "libcamera")]
    Libcamera {
        tx: std::sync::mpsc::Sender<ControlMessage>,
        pending: std::sync::Arc<std::sync::Mutex<PendingControlState>>,
    },
    #[cfg(feature = "file-backend")]
    File {
        state: file_backend::FileControlStateHandle,
    },
    Virtual,
}

/// Unified capture handle; currently backed by a bounded queue and a worker thread.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let device = probe_all().into_iter().next().expect("device");
/// let handle = CaptureRequest::new(&device).start()?;
/// match handle.recv() {
///     RecvOutcome::Data(frame) => println!("frame {:?}", frame.meta().format),
///     RecvOutcome::Empty => {}
///     RecvOutcome::Closed => {}
/// }
/// handle.stop();
/// # Ok::<(), styx::capture_api::CaptureError>(())
/// ```
pub struct CaptureHandle {
    pub(crate) backend: BackendKind,
    pub(crate) control: ControlPlane,
    pub(crate) descriptor: CaptureDescriptor,
    pub(crate) mode: Mode,
    pub(crate) interval: Option<Interval>,
    pub(crate) rx: BoundedRx<FrameLease>,
    pub(crate) stop_tx: Option<std::sync::mpsc::Sender<()>>,
    pub(super) worker: Option<WorkerHandle>,
    pub(crate) metrics: StageMetrics,
}

/// Worker handle for capture backends.
pub enum WorkerHandle {
    Thread(std::thread::JoinHandle<()>),
    #[cfg(feature = "async")]
    Async(tokio::task::JoinHandle<()>),
}

impl CaptureHandle {
    /// Receive a frame from the capture queue.
    ///
    /// Returns `RecvOutcome::Empty` when the queue is temporarily empty.
    pub fn recv(&self) -> RecvOutcome<FrameLease> {
        let start = Instant::now();
        match self.rx.recv() {
            RecvOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                RecvOutcome::Data(frame)
            }
            other => other,
        }
    }

    /// Async receive helper when the `async` feature is enabled.
    #[cfg(feature = "async")]
    pub async fn recv_async(&self) -> RecvOutcome<FrameLease> {
        let start = Instant::now();
        match self.rx.recv_async().await {
            RecvOutcome::Data(frame) => {
                self.metrics.record(start.elapsed());
                RecvOutcome::Data(frame)
            }
            other => other,
        }
    }

    /// Blocking receive with a configurable sleep to avoid busy-waiting.
    pub fn recv_blocking(&self, wait: std::time::Duration) -> RecvOutcome<FrameLease> {
        loop {
            match self.recv() {
                RecvOutcome::Empty => {
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                    } else {
                        std::thread::yield_now();
                    }
                }
                other => return other,
            }
        }
    }

    /// Stop the capture worker.
    pub fn stop(mut self) {
        self.teardown_in_place();
    }

    /// Stop the capture worker without consuming the handle.
    pub fn stop_in_place(&mut self) {
        self.teardown_in_place();
    }

    /// Async variant of stop (uses the current runtime when present).
    #[cfg(feature = "async")]
    pub async fn stop_async(self) {
        tokio::task::block_in_place(|| self.stop());
    }

    fn teardown_in_place(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.rx.close();
        if let Some(worker) = self.worker.take() {
            match worker {
                WorkerHandle::Thread(h) => {
                    let _ = h.join();
                }
                #[cfg(feature = "async")]
                WorkerHandle::Async(h) => {
                    if let Ok(rt) = tokio::runtime::Handle::try_current() {
                        let _ = rt.block_on(h);
                    } else {
                        h.abort();
                    }
                }
            }
        }
    }

    /// Reconfigure capture by stopping this session and starting a new one from a request.
    ///
    /// This consumes the old handle and returns a fresh one.
    pub fn reconfigure(
        self,
        request: super::CaptureRequest<'_>,
    ) -> Result<CaptureHandle, CaptureError> {
        let mut handle = self;
        handle.reconfigure_in_place(request)?;
        Ok(handle)
    }

    /// Reconfigure capture in-place by stopping the current worker before starting a new one.
    ///
    /// This fully restarts the camera, which is required when changing resolution or pixel formats.
    pub fn reconfigure_in_place(
        &mut self,
        request: super::CaptureRequest<'_>,
    ) -> Result<(), CaptureError> {
        self.teardown_in_place();
        let mut new_capture = request.start()?;
        // Swap to avoid double-drop; the torn-down handle will drop harmlessly.
        mem::swap(self, &mut new_capture);
        Ok(())
    }

    /// Backend kind used for this capture.
    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// Mode in use.
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Interval in use (if advertised).
    pub fn interval(&self) -> Option<Interval> {
        self.interval
    }

    /// Capture timing metrics (per-frame wait/receive durations).
    pub fn metrics(&self) -> StageMetrics {
        self.metrics.clone()
    }

    /// Apply a control to the active backend (best-effort).
    ///
    /// # Example
    /// ```rust,ignore
    /// use styx::prelude::*;
    ///
    /// let device = probe_all().into_iter().next().expect("device");
    /// let handle = CaptureRequest::new(&device).start()?;
    /// let _ = handle.set_control(ControlId(0), ControlValue::None);
    /// # Ok::<(), styx::capture_api::CaptureError>(())
    /// ```
    pub fn set_control(&self, _id: ControlId, _value: ControlValue) -> Result<(), CaptureError> {
        match &self.control {
            ControlPlane::None | ControlPlane::Virtual => Err(CaptureError::ControlUnsupported),
            #[cfg(feature = "v4l2")]
            ControlPlane::V4l2 { path } => apply_v4l2_controls(path, &[(_id, _value)]),
            #[cfg(feature = "libcamera")]
            ControlPlane::Libcamera { tx, pending } => {
                {
                    let mut guard = pending.lock().map_err(|_| CaptureError::ControlApply("libcamera pending lock poisoned".into()))?;
                    if matches!(_value, ControlValue::None) {
                        guard.insert(_id, None);
                    } else {
                        guard.insert(_id, Some(_value));
                    }
                }
                tx.send(ControlMessage::Wake)
                    .map_err(|_| CaptureError::ControlApply("libcamera channel closed".into()))
            }
            #[cfg(feature = "file-backend")]
            ControlPlane::File { state } => file_backend::apply_file_control(state, _id, _value),
        }
    }

    /// Async wrapper for set_control.
    #[cfg(feature = "async")]
    pub async fn set_control_async(
        &self,
        id: ControlId,
        value: ControlValue,
    ) -> Result<(), CaptureError> {
        tokio::task::block_in_place(|| self.set_control(id, value))
    }

    /// Fetch a control value when supported (V4L2).
    pub fn get_control(&self, _id: ControlId) -> Result<ControlValue, CaptureError> {
        match &self.control {
            #[cfg(feature = "v4l2")]
            ControlPlane::V4l2 { path } => read_v4l2_control(path, _id),
            #[cfg(feature = "libcamera")]
            ControlPlane::Libcamera { tx, .. } => {
                let (resp_tx, resp_rx) = std::sync::mpsc::channel();
                tx.send(ControlMessage::Get(_id, resp_tx))
                    .map_err(|_| CaptureError::ControlApply("libcamera channel closed".into()))?;
                resp_rx
                    .recv()
                    .map_err(|_| CaptureError::ControlApply("libcamera response closed".into()))?
            }
            #[cfg(feature = "file-backend")]
            ControlPlane::File { state } => file_backend::read_file_control(state, _id),
            _ => Err(CaptureError::ControlUnsupported),
        }
    }

    /// Async wrapper for get_control.
    #[cfg(feature = "async")]
    pub async fn get_control_async(&self, id: ControlId) -> Result<ControlValue, CaptureError> {
        tokio::task::block_in_place(|| self.get_control(id))
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // If the consumer forgot to call stop, attempt a best-effort shutdown to avoid leaks.
        if self.worker.is_some() {
            self.teardown_in_place();
        }
    }
}

impl CaptureSource for CaptureHandle {
    fn descriptor(&self) -> &CaptureDescriptor {
        &self.descriptor
    }

    fn next_frame(&self) -> Option<FrameLease> {
        loop {
            match self.rx.recv() {
                RecvOutcome::Data(frame) => return Some(frame),
                RecvOutcome::Closed => return None,
                RecvOutcome::Empty => {
                    std::thread::yield_now();
                    continue;
                }
            }
        }
    }
}

pub(crate) fn start_backend(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    #[allow(unused_variables)] controls: Vec<(ControlId, ControlValue)>,
    #[allow(unused_variables)] tdn_output_mode: TdnOutputMode,
) -> Result<CaptureHandle, CaptureError> {
    let descriptor = backend.descriptor.clone();
    match backend.kind {
        BackendKind::Virtual => virtual_backend::start_virtual(mode, interval, descriptor),
        #[cfg(feature = "v4l2")]
        BackendKind::V4l2 => {
            v4l2_backend::start_v4l2(backend, mode, interval, controls, descriptor)
        }
        #[cfg(not(feature = "v4l2"))]
        BackendKind::V4l2 => Err(CaptureError::BackendMissing(BackendKind::V4l2)),
        #[cfg(feature = "libcamera")]
        BackendKind::Libcamera => {
            libcamera_backend::start_libcamera(backend, mode, interval, controls, descriptor, tdn_output_mode)
        }
        #[cfg(not(feature = "libcamera"))]
        BackendKind::Libcamera => Err(CaptureError::BackendMissing(BackendKind::Libcamera)),
        #[cfg(feature = "netcam")]
        BackendKind::Netcam => netcam_backend::start_netcam(backend, mode, interval, descriptor),
        #[cfg(not(feature = "netcam"))]
        BackendKind::Netcam => Err(CaptureError::BackendMissing(BackendKind::Netcam)),
        #[cfg(feature = "file-backend")]
        BackendKind::File => {
            file_backend::start_file(backend, mode, interval, controls, descriptor)
        }
        #[cfg(not(feature = "file-backend"))]
        BackendKind::File => Err(CaptureError::BackendMissing(BackendKind::File)),
    }
}

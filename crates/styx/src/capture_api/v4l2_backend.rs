use std::mem;
use std::os::fd::{FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smallvec::smallvec;
use styx_core::prelude::*;
use v4l::buffer::Metadata as V4l2Metadata;
use v4l::buffer::Type;
use v4l::device::Handle;
use v4l::memory::Memory;
use v4l::v4l_sys::*;
use v4l::v4l2;
use v4l::{format::FourCC, prelude::*, video::Capture as _};

use crate::capture_api::controls::apply_v4l2_controls;
use crate::capture_api::handle::enqueue_capture_frame;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, StyxConfig, WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

struct V4l2MappedBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: each mapped buffer is owned by its `V4l2MmapManager`; moving the descriptor between
// threads does not duplicate ownership, and unmapping is centralized in the manager drop path.
unsafe impl Send for V4l2MappedBuffer {}

// SAFETY: shared references expose immutable frame views only. Queue/dequeue state is protected by
// the manager mutex, and the mapping lifetime is tied to the manager.
unsafe impl Sync for V4l2MappedBuffer {}

struct V4l2MmapManager {
    handle: Arc<Handle>,
    buf_type: Type,
    buffers: Vec<V4l2MappedBuffer>,
    state: Mutex<V4l2MmapState>,
}

struct V4l2MmapState {
    active: bool,
    queued: Vec<bool>,
    checked_out: Vec<bool>,
    timeout_ms: i32,
}

struct V4l2MmapBacking {
    manager: Arc<V4l2MmapManager>,
    recycle_tx: Sender<usize>,
    index: Mutex<Option<usize>>,
    tracker: Arc<ExternalBackingTracker>,
    bytes: usize,
}

impl V4l2MmapManager {
    fn new(
        handle: Arc<Handle>,
        buf_type: Type,
        count: u32,
        timeout: Duration,
    ) -> std::io::Result<Arc<Self>> {
        let mut reqbufs = v4l2_requestbuffers {
            count,
            type_: buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                handle.fd(),
                v4l2::vidioc::VIDIOC_REQBUFS,
                &mut reqbufs as *mut _ as *mut std::os::raw::c_void,
            )?;
        }

        let mut buffers = Vec::with_capacity(reqbufs.count as usize);
        for index in 0..reqbufs.count {
            let mut v4l2_buf = v4l2_buffer {
                index,
                type_: buf_type as u32,
                memory: Memory::Mmap as u32,
                ..unsafe { mem::zeroed() }
            };
            unsafe {
                v4l2::ioctl(
                    handle.fd(),
                    v4l2::vidioc::VIDIOC_QUERYBUF,
                    &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
                )?;
                let ptr = v4l2::mmap(
                    std::ptr::null_mut(),
                    v4l2_buf.length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    handle.fd(),
                    v4l2_buf.m.offset as libc::off_t,
                )?;
                let ptr = NonNull::new(ptr.cast::<u8>())
                    .ok_or_else(|| std::io::Error::other("v4l2 mmap returned null"))?;
                buffers.push(V4l2MappedBuffer {
                    ptr,
                    len: v4l2_buf.length as usize,
                });
            }
        }

        Ok(Arc::new(Self {
            handle,
            buf_type,
            buffers,
            state: Mutex::new(V4l2MmapState {
                active: false,
                queued: vec![false; reqbufs.count as usize],
                checked_out: vec![false; reqbufs.count as usize],
                timeout_ms: timeout.as_millis().try_into().unwrap_or(i32::MAX),
            }),
        }))
    }

    fn dequeue(&self) -> std::io::Result<(usize, V4l2Metadata)> {
        let timeout_ms = {
            let mut state = self.state.lock().unwrap();
            if !state.active {
                for index in 0..self.buffers.len() {
                    if !state.queued[index] && !state.checked_out[index] {
                        self.queue_locked(index, &mut state)?;
                    }
                }
                self.stream_on_locked(&mut state)?;
            }
            state.timeout_ms
        };

        if self.handle.poll(libc::POLLIN, timeout_ms)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "VIDIOC_DQBUF",
            ));
        }

        let mut v4l2_buf = v4l2_buffer {
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_DQBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        let index = v4l2_buf.index as usize;
        let mut state = self.state.lock().unwrap();
        state.queued[index] = false;
        state.checked_out[index] = true;
        Ok((
            index,
            V4l2Metadata {
                bytesused: v4l2_buf.bytesused,
                flags: v4l2_buf.flags.into(),
                field: v4l2_buf.field,
                timestamp: v4l2_buf.timestamp.into(),
                sequence: v4l2_buf.sequence,
            },
        ))
    }

    fn recycle(&self, index: usize) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if index >= state.checked_out.len() {
            return Ok(());
        }
        state.checked_out[index] = false;
        if state.active {
            self.queue_locked(index, &mut state)?;
        }
        Ok(())
    }

    fn mapped_plane(&self, index: usize) -> Option<&[u8]> {
        let buffer = self.buffers.get(index)?;
        Some(unsafe { std::slice::from_raw_parts(buffer.ptr.as_ptr(), buffer.len) })
    }

    fn mapped_bytes(&self, index: usize) -> Option<usize> {
        self.buffers.get(index).map(|buffer| buffer.len)
    }

    fn export_dmabuf(&self, index: usize) -> std::io::Result<OwnedFd> {
        let mut expbuf = v4l2_exportbuffer {
            type_: self.buf_type as u32,
            index: index as u32,
            plane: 0,
            flags: libc::O_CLOEXEC as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_EXPBUF,
                &mut expbuf as *mut _ as *mut std::os::raw::c_void,
            )?;
            Ok(OwnedFd::from_raw_fd(expbuf.fd))
        }
    }

    fn stop_stream(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        self.stop_stream_locked(&mut state)
    }

    fn stream_on_locked(&self, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if state.active {
            return Ok(());
        }
        let mut typ = self.buf_type as u32;
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMON,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.active = true;
        Ok(())
    }

    fn stop_stream_locked(&self, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if !state.active {
            return Ok(());
        }
        let mut typ = self.buf_type as u32;
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMOFF,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.active = false;
        for queued in &mut state.queued {
            *queued = false;
        }
        Ok(())
    }

    fn queue_locked(&self, index: usize, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if state.queued[index] {
            return Ok(());
        }
        let mut v4l2_buf = v4l2_buffer {
            index: index as u32,
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_QBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.queued[index] = true;
        Ok(())
    }
}

impl Drop for V4l2MmapManager {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = self.stop_stream_locked(&mut state);
        }
        for buffer in &self.buffers {
            unsafe {
                let _ = v4l2::munmap(buffer.ptr.as_ptr().cast(), buffer.len);
            }
        }
        let mut reqbufs = v4l2_requestbuffers {
            count: 0,
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            let _ = v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_REQBUFS,
                &mut reqbufs as *mut _ as *mut std::os::raw::c_void,
            );
        }
    }
}

impl V4l2MmapBacking {
    fn new(
        manager: Arc<V4l2MmapManager>,
        recycle_tx: Sender<usize>,
        index: usize,
        tracker: Arc<ExternalBackingTracker>,
        bytes: usize,
    ) -> Arc<Self> {
        tracker.acquire(bytes);
        Arc::new(Self {
            manager,
            recycle_tx,
            index: Mutex::new(Some(index)),
            tracker,
            bytes,
        })
    }
}

impl ExternalBacking for V4l2MmapBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        match index {
            0 => {
                // The manager is held by `Arc` inside this backing, so the mmap remains alive
                // while any `FrameLease` borrowing this external backing exists.
                let buffer_index = *self.index.lock().unwrap();
                self.manager.mapped_plane(buffer_index?)
            }
            _ => None,
        }
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "v4l2_mmap"
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let Some(index) = *self.index.lock().unwrap() else {
            return Err(FrameExportError::InvalidDescriptor);
        };
        let fd = self
            .manager
            .export_dmabuf(index)
            .map_err(FrameExportError::Fd)?;
        Ok(Some(FrameBackingExport::DmabufPlanes {
            planes: vec![FrameFdPlane {
                fd,
                offset: 0,
                len: self.bytes,
            }],
        }))
    }
}

impl Drop for V4l2MmapBacking {
    fn drop(&mut self) {
        self.tracker.release(self.bytes);
        if let Some(index) = self.index.lock().unwrap().take() {
            // Recycle only when the final external backing reference drops. This prevents the
            // worker from requeueing/unmapping a V4L2 buffer while a `FrameLease` still exposes it.
            let _ = self.recycle_tx.send(index);
        }
    }
}

fn drain_recycled_buffers(manager: &V4l2MmapManager, recycle_rx: &Receiver<usize>) {
    while let Ok(index) = recycle_rx.try_recv() {
        let _ = manager.recycle(index);
    }
}

fn is_encoded_bitstream(code: FourCc) -> bool {
    code.is_compressed()
}

fn supports_v4l2_mmap_zero_copy(code: FourCc) -> bool {
    matches!(
        &code.to_u32().to_le_bytes(),
        b"MJPG" | b"JPEG" | b"YUYV" | b"RG24" | b"RGB3" | b"BGR3" | b"RGBA" | b"BGRA"
    )
}

fn build_v4l2_single_plane_layout(
    encoded: bool,
    height: usize,
    stride: usize,
    bytes_used: usize,
) -> Option<PlaneLayout> {
    if encoded {
        return Some(PlaneLayout {
            offset: 0,
            len: bytes_used,
            stride: bytes_used.max(1),
        });
    }
    if height == 0 || stride == 0 {
        return None;
    }
    let required = height.saturating_mul(stride);
    if bytes_used < required {
        return None;
    }
    Some(PlaneLayout {
        offset: 0,
        len: required,
        stride,
    })
}

#[derive(Debug, Clone, Copy)]
struct V4l2SinglePlaneLayoutPlan {
    layout: PlaneLayout,
    zero_copy_safe: bool,
}

fn plan_v4l2_single_plane_layout(
    code: FourCc,
    width: usize,
    height: usize,
    negotiated_stride: usize,
    negotiated_size: usize,
    mapped_len: usize,
    bytes_used: usize,
) -> Option<V4l2SinglePlaneLayoutPlan> {
    if bytes_used == 0 || bytes_used > mapped_len {
        return None;
    }

    let encoded = is_encoded_bitstream(code);
    if encoded {
        let layout = build_v4l2_single_plane_layout(true, height, 0, bytes_used)?;
        return Some(V4l2SinglePlaneLayoutPlan {
            layout,
            zero_copy_safe: layout.len <= mapped_len,
        });
    }

    let min_stride = min_stride_for_fourcc(code, width).max(1);
    let inferred_stride = if height > 0 { bytes_used / height } else { 0 };
    let stride = negotiated_stride.max(inferred_stride).max(min_stride);
    let required = height.checked_mul(stride)?;
    if required == 0 || bytes_used < required {
        return None;
    }

    let layout = build_v4l2_single_plane_layout(false, height, stride, bytes_used)?;
    let advertised_capacity_ok = negotiated_size == 0 || layout.len <= negotiated_size;
    Some(V4l2SinglePlaneLayoutPlan {
        layout,
        zero_copy_safe: advertised_capacity_ok && layout.len <= mapped_len,
    })
}

fn min_stride_for_fourcc(code: FourCc, width: usize) -> usize {
    match &code.to_u32().to_le_bytes() {
        // MIPI packed RAW10/RAW12 bayer.
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" => width.div_ceil(4) * 5,
        b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" => width.div_ceil(2) * 3,

        // 8-bit bayer.
        b"BA81" | b"RGGB" | b"GRBG" | b"GBRG" | b"BGGR" => width,

        // 10/12/14/16-bit bayer (stored in 16-bit words) and mono16.
        b"BA10" | b"BA12" | b"BA14" | b"BG10" | b"BG12" | b"BG14" | b"BG16" | b"GB10" | b"GB12"
        | b"GB14" | b"GB16" | b"RG10" | b"RG12" | b"RG14" | b"RG16" | b"GR10" | b"GR12"
        | b"GR14" | b"GR16" | b"BYR2" | b"R16 " => width.saturating_mul(2),

        // Common packed YUV/RGB defaults.
        b"YUYV" => width.saturating_mul(2),
        b"NV12" => width, // luma plane; backend uses bytesused/stride anyway
        b"RG24" | b"RGB3" | b"BGR3" => width.saturating_mul(3),
        b"RGBA" | b"BGRA" | b"RGB0" | b"BGR0" => width.saturating_mul(4),
        _ => width.saturating_mul(3),
    }
}

pub(super) fn start_v4l2(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
) -> Result<CaptureHandle, CaptureError> {
    let path = match &backend.handle {
        BackendHandle::V4l2 { path } => path.clone(),
        _ => return Err(CaptureError::Backend("v4l2 path missing".into())),
    };

    let dev = Device::with_path(&path).map_err(|e| CaptureError::Backend(e.to_string()))?;

    let repr = mode.format.code.to_u32().to_le_bytes();
    let fourcc = FourCC::new(&repr);
    let mut fmt = dev
        .format()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    fmt.width = mode.format.resolution.width.get();
    fmt.height = mode.format.resolution.height.get();
    fmt.fourcc = fourcc;
    let fmt = dev
        .set_format(&fmt)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let negotiated_code = FourCc::new(fmt.fourcc.repr);
    let negotiated_resolution = Resolution::new(fmt.width, fmt.height)
        .ok_or_else(|| CaptureError::Backend("v4l2 negotiated zero-sized frame".into()))?;
    let negotiated_format =
        MediaFormat::new(negotiated_code, negotiated_resolution, mode.format.color);
    let mode = Mode {
        id: mode.id,
        format: negotiated_format,
        intervals: mode.intervals,
        interval_stepwise: mode.interval_stepwise,
    };

    if let Some(iv) = interval {
        let mut params = dev
            .params()
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        params.interval.numerator = iv.numerator.get();
        params.interval.denominator = iv.denominator.get();
        dev.set_params(&params)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
    }

    if !controls.is_empty() {
        apply_v4l2_controls(&path, &controls)?;
    }

    let width = fmt.width as usize;
    let height = fmt.height as usize;
    let encoded = is_encoded_bitstream(mode.format.code);
    let min_stride = min_stride_for_fourcc(mode.format.code, width);
    let negotiated_stride_bytes = if encoded {
        0
    } else if fmt.stride > 0 {
        (fmt.stride as usize).max(min_stride)
    } else {
        min_stride.max(1)
    };
    let negotiated_size = fmt.size as usize;
    let frame_capacity = if encoded {
        negotiated_size
            .max(256 * 1024)
            .max(width.saturating_mul(height))
    } else {
        height
            .saturating_mul(negotiated_stride_bytes)
            .max(negotiated_size)
            .max(width.saturating_mul(fmt.height as usize).saturating_mul(3))
    };
    tracing::debug!(
        backend = "v4l2",
        path = %path,
        width = fmt.width,
        height = fmt.height,
        fourcc = ?mode.format.code,
        stride_bytes = negotiated_stride_bytes,
        buffer_size = negotiated_size,
        frame_capacity,
        encoded,
        "v4l2 negotiated capture format"
    );
    let capture_tunables = config.capture_tunables();
    let v4l2_config = config.v4l2_config();
    let pool_limits = capture_tunables.pool_limits(4, frame_capacity, 8);
    let manager = V4l2MmapManager::new(
        dev.handle(),
        Type::VideoCapture,
        4,
        Duration::from_millis(v4l2_config.mmap_poll_ms),
    )
    .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let queue_depth = capture_tunables.queue_depth;
    let (tx, rx) = bounded(queue_depth);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<usize>();
    let mode_clone = mode.clone();
    let backing_tracker = Arc::new(ExternalBackingTracker::new("v4l2_mmap"));
    let manager_for_worker = Arc::clone(&manager);
    let tracker_for_worker = Arc::clone(&backing_tracker);
    let worker = thread::spawn(move || {
        let send_timeout = Duration::from_millis(v4l2_config.send_timeout_ms);
        let error_backoff = Duration::from_millis(v4l2_config.error_backoff_ms);
        let zero_copy_requested = supports_v4l2_mmap_zero_copy(mode_clone.format.code);
        let shared_pool =
            SharedBufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
        let height = mode_clone.format.resolution.height.get() as usize;
        let width = mode_clone.format.resolution.width.get() as usize;
        loop {
            drain_recycled_buffers(&manager_for_worker, &recycle_rx);
            if stop_rx.try_recv().is_ok() {
                drain_recycled_buffers(&manager_for_worker, &recycle_rx);
                let _ = manager_for_worker.stop_stream();
                break;
            }
            match manager_for_worker.dequeue() {
                Ok((index, meta)) => {
                    let mapped_len = manager_for_worker.mapped_bytes(index).unwrap_or_default();
                    let bytes_used = (meta.bytesused as usize).min(mapped_len);
                    let Some(layout_plan) = plan_v4l2_single_plane_layout(
                        mode_clone.format.code,
                        width,
                        height,
                        negotiated_stride_bytes,
                        negotiated_size,
                        mapped_len,
                        bytes_used,
                    ) else {
                        let _ = manager_for_worker.recycle(index);
                        continue;
                    };
                    let zero_copy_enabled = zero_copy_requested && layout_plan.zero_copy_safe;
                    let ts = std::time::Duration::from(meta.timestamp)
                        .as_nanos()
                        .min(u64::MAX as u128) as u64;
                    let meta = FrameMeta::new(mode_clone.format, ts)
                        .with_capture_instant(std::time::Instant::now())
                        .with_transition(ResidencyTransition {
                            from: if zero_copy_enabled {
                                FrameResidency::HostExternal
                            } else if is_encoded_bitstream(mode_clone.format.code) {
                                FrameResidency::CompressedPacket
                            } else {
                                FrameResidency::HostOwned
                            },
                            to: if zero_copy_enabled {
                                FrameResidency::HostExternal
                            } else if is_encoded_bitstream(mode_clone.format.code) {
                                FrameResidency::CompressedPacket
                            } else {
                                FrameResidency::HostExternal
                            },
                            reason: ResidencyTransitionReason::Capture,
                            copied: !zero_copy_enabled,
                        })
                        .with_backend(BackendFrameMeta::V4l2(V4l2FrameMeta {
                            sequence: meta.sequence,
                            bytes_used: meta.bytesused,
                            field: meta.field,
                            flags: u32::from(meta.flags),
                            zero_copy: zero_copy_enabled,
                        }));
                    let layout = layout_plan.layout;
                    let frame = if zero_copy_enabled {
                        let backing = V4l2MmapBacking::new(
                            Arc::clone(&manager_for_worker),
                            recycle_tx.clone(),
                            index,
                            Arc::clone(&tracker_for_worker),
                            bytes_used,
                        );
                        FrameLease::from_external(meta, smallvec![layout], backing)
                    } else {
                        let Ok(pool) = &shared_pool else {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        };
                        let Ok(mut lease) = pool.lease() else {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        };
                        if lease.try_resize(bytes_used).is_err() {
                            let _ = manager_for_worker.recycle(index);
                            continue;
                        }
                        if let Some(src) = manager_for_worker.mapped_plane(index) {
                            lease.as_mut_slice()[..bytes_used].copy_from_slice(&src[..bytes_used]);
                        }
                        let _ = manager_for_worker.recycle(index);
                        match FrameLease::single_plane_shared(
                            meta,
                            lease,
                            layout.len,
                            layout.stride,
                        ) {
                            Ok(frame) => frame,
                            Err(_) => continue,
                        }
                    };
                    if enqueue_capture_frame(&tx, frame, "v4l2", send_timeout) {
                        let _ = manager_for_worker.stop_stream();
                        break;
                    }
                }
                Err(err) => {
                    // Timeouts are expected due to the short poll timeout above.
                    if err.kind() != std::io::ErrorKind::TimedOut {
                        thread::sleep(error_backoff);
                    }
                }
            }
        }
    });

    Ok(CaptureHandle {
        backend: BackendKind::V4l2,
        control: ControlPlane::V4l2 { path },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: vec![backing_tracker],
        worker_error: std::sync::Arc::new(std::sync::Mutex::new(None)),
        control_error: std::sync::Arc::new(std::sync::Mutex::new(None)),
    })
}

#[cfg(test)]
mod tests;

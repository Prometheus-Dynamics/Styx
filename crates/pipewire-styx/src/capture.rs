//! A Styx capture on its own thread, started when a PipeWire consumer connects and stopped when
//! it goes. Frames come from the camera in this process (through the Styx planner) or from a
//! Styx camera service.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use styx::capture_api::CaptureBuffers;
use styx::ipc::FrameClient;
use styx::planner::plan_frames;
use styx::prelude::*;

/// Where a camera's frames come from.
#[derive(Clone)]
pub enum Source {
    Local(Box<ProbedDevice>),
    /// A camera service socket and the camera's name there.
    Service {
        path: String,
        camera: String,
    },
}

/// What a PipeWire consumer negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction (`0/1` = any).
    pub rate: (u32, u32),
}

impl Request {
    pub fn requirements(&self, exact_size: bool) -> FrameRequirements {
        let mut req =
            FrameRequirements::formats([self.fourcc]).output_resolution(self.width, self.height);
        if exact_size {
            req = req
                .min_resolution(self.width, self.height)
                .max_resolution(self.width, self.height);
        }
        let (num, den) = self.rate;
        if num > 0 && den > 0 {
            req = req.min_fps((num / den).max(1));
        }
        req
    }
}

/// A running capture; dropping it stops the thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Start capturing `request` from `source`, calling `deliver` with every frame on the
    /// capture thread and `failed` once if the capture cannot start or ends. A camera in this
    /// process captures into `buffers` where it can (frames passed through unchanged, a backend
    /// that imports buffers).
    pub fn start(
        source: Source,
        request: Request,
        buffers: Option<CaptureBuffers>,
        mut deliver: impl FnMut(FrameLease) + Send + 'static,
        failed: impl FnOnce(String) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("styx-pw-capture".into())
            .spawn(move || {
                let result = run(&source, &request, buffers, &stopping, &mut deliver);
                if let Err(err) = result
                    && !stopping.load(Ordering::Relaxed)
                {
                    failed(err);
                }
            })
            .ok();
        Self { stop, thread }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const POLL: Duration = Duration::from_millis(100);

fn run(
    source: &Source,
    request: &Request,
    buffers: Option<CaptureBuffers>,
    stop: &AtomicBool,
    deliver: &mut dyn FnMut(FrameLease),
) -> Result<(), String> {
    match source {
        Source::Local(device) => {
            let mut plan =
                plan_frames(device, &request.requirements(true)).map_err(|e| e.to_string())?;
            let (num, den) = request.rate;
            if let Some(interval) = plan.mode.intervals.iter().copied().find(|i| {
                u64::from(i.denominator.get()) * u64::from(den)
                    == u64::from(i.numerator.get()) * u64::from(num)
            }) {
                plan.interval = Some(interval);
            }
            if let Some(buffers) = buffers {
                plan = plan.capture_into(buffers);
            }
            let mut frames = plan.start().map_err(|e| e.to_string())?;
            while !stop.load(Ordering::Relaxed) {
                match frames.next_frame(POLL) {
                    RecvOutcome::Data(frame) => deliver(frame),
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => return Err("the camera stopped".into()),
                }
            }
            frames.stop();
            Ok(())
        }
        Source::Service { path, camera } => {
            let client = FrameClient::request_camera(path, camera, &request.requirements(false))
                .map_err(|e| format!("camera service {path}: {e}"))?
                .reconnecting();
            while !stop.load(Ordering::Relaxed) {
                match client.recv(POLL) {
                    RecvOutcome::Data(frame) => deliver(frame),
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => return Err("the camera service went away".into()),
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn captures_until_dropped() {
        let device = CaptureRequest::virtual_source(
            VirtualSourceConfig::new()
                .name("virtual")
                .format(FourCc::YUYV)
                .resolution(64, 48)
                .fps(30),
        )
        .into_device();
        let (tx, rx) = mpsc::channel();
        let capture = Capture::start(
            Source::Local(Box::new(device)),
            Request {
                fourcc: FourCc::RG24,
                width: 64,
                height: 48,
                rate: (30, 1),
            },
            None,
            move |frame| {
                let _ = tx.send(frame.meta().format.code);
            },
            |err| panic!("capture failed: {err}"),
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(FourCc::RG24));
        drop(capture);
    }
}

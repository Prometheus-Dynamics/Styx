//! One USB camera, streamed through Lemnos: attach, stream, detach, and come back.
//!
//! The camera is found by tag (`camera.frames/1`) and name. Attaching selects a format, opens the
//! frame stream the camera names, and starts it. While streaming, the device list is checked every
//! [`HEALTH_INTERVAL`]: a device that is gone, not running or failed is detached. A camera that is
//! absent, detached or failing is retried on the [`Backoff`] schedule, forever, until
//! [`UsbCamera::stop`]. Nothing gives up and nothing turns the camera off by itself.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use lemnos_client::{DeviceHealth, DeviceState, RemoteCaller};
use lemnos_contract_camera_frames::{CameraFrames, Counters, FrameFormat, MAX_STREAM_NAME};
use lemnos_core::{Bound, Status};
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_lemnos::{Client, Frames};

use crate::backoff::Backoff;
use crate::choice::{Choice, Preference, choose};
use crate::device::find_device;

/// How often a streaming camera's device is checked in the service's device list.
pub const HEALTH_INTERVAL: Duration = Duration::from_millis(250);

/// What to stream and how to choose it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The `camera.frames/1` device's name. `None` takes the only one.
    pub device: Option<String>,
    /// Which format to stream.
    pub preference: Preference,
}

/// Where the camera is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Not wanted: [`UsbCamera::stop`] was called, or [`UsbCamera::start`] was not.
    Off,
    /// Wanted, not streaming. The next attach is at [`CameraStatus::retry_in`].
    Waiting,
    /// Streaming.
    Streaming,
}

/// The camera's state, read without touching the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraStatus {
    pub phase: Phase,
    /// The device attached, or the one being waited for when it is named.
    pub device: Option<String>,
    /// The format streaming, when streaming.
    pub format: Option<FrameFormat>,
    /// Failed attaches and losses since the last success.
    pub failures: u32,
    /// Time until the next attach, when waiting.
    pub retry_in: Option<Duration>,
    /// Why the last attach or stream failed. Kept until the next success.
    pub last_error: Option<String>,
    /// Frames handed out since the camera was created.
    pub frames: u64,
    /// Successful attaches since the camera was created.
    pub attaches: u64,
}

/// A streaming camera's live parts.
struct Live {
    device: String,
    camera: Bound<dyn CameraFrames, RemoteCaller>,
    frames: Frames,
    format: FrameFormat,
    checked: Instant,
}

enum State {
    Off,
    Waiting { until: Instant },
    Streaming(Box<Live>),
}

/// A USB camera on the native path.
///
/// Frames come from [`next_frame`](Self::next_frame) as [`FrameLease`]s over the memory Lemnos
/// filled, with the camera's timestamps and identities. Frames in a format of this crate's
/// choosing are uncompressed; compressed formats are not chosen (see [`crate::choose`]).
pub struct UsbCamera {
    client: Client,
    settings: Settings,
    state: State,
    wanted: bool,
    backoff: Backoff,
    last_error: Option<String>,
    retry_in: Option<Duration>,
    frames: u64,
    attaches: u64,
}

impl fmt::Debug for UsbCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbCamera")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn status_text(what: &str, status: Status) -> String {
    format!("{what}: status {}", status.0)
}

fn pause_until(at: Instant) {
    let rest = at.saturating_duration_since(Instant::now());
    if !rest.is_zero() {
        thread::sleep(rest);
    }
}

/// Whether the named device is still listed, running and not failed.
fn check(client: &Client, name: &str) -> Result<(), String> {
    let devices = client.list().map_err(|e| format!("list: {e}"))?;
    let device = devices
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| format!("{name}: gone from the device list"))?;
    if device.state != DeviceState::Running {
        return Err(format!("{name}: {:?}: {}", device.state, device.reason));
    }
    if device.health == DeviceHealth::Failed {
        return Err(format!("{name}: failed: {}", device.reason));
    }
    Ok(())
}

impl UsbCamera {
    /// A camera that is not started. Nothing is attached until [`start`](Self::start).
    pub fn new(client: Client, settings: Settings) -> Self {
        Self {
            client,
            settings,
            state: State::Off,
            wanted: false,
            backoff: Backoff::default(),
            last_error: None,
            retry_in: None,
            frames: 0,
            attaches: 0,
        }
    }

    /// Wants the camera streaming, and tries to attach now. A failure is not an error here: it is
    /// in [`status`](Self::status), and the camera is retried.
    pub fn start(&mut self) {
        self.wanted = true;
        if matches!(self.state, State::Off) {
            self.attach();
        }
    }

    /// Stops the camera and does not restart it until [`start`](Self::start).
    pub fn stop(&mut self) {
        self.wanted = false;
        self.detach();
        self.retry_in = None;
    }

    /// The camera's state now.
    pub fn status(&self) -> CameraStatus {
        let (phase, device, format, retry_in) = match &self.state {
            State::Off => (Phase::Off, None, None, None),
            State::Waiting { until } => (
                Phase::Waiting,
                None,
                None,
                Some(until.saturating_duration_since(Instant::now())),
            ),
            State::Streaming(live) => (
                Phase::Streaming,
                Some(live.device.clone()),
                Some(live.format),
                None,
            ),
        };
        CameraStatus {
            phase,
            device: device.or_else(|| self.settings.device.clone()),
            format,
            failures: self.backoff.failures(),
            retry_in,
            last_error: self.last_error.clone(),
            frames: self.frames,
            attaches: self.attaches,
        }
    }

    /// The camera's own counters since its last start, when streaming.
    pub fn counters(&mut self) -> Option<Counters> {
        match &mut self.state {
            State::Streaming(live) => live.camera.counters().ok(),
            _ => None,
        }
    }

    /// Waits up to `timeout` for a frame. Returns `None` when none came in time, or when the camera
    /// is not streaming (it is attached or retried meanwhile, in this call).
    ///
    /// Call it in a loop. A lost camera is detached and retried inside these calls, so the loop
    /// needs no recovery code of its own.
    pub fn next_frame(&mut self, timeout: Duration) -> Option<FrameLease<usize>> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.wanted {
                pause_until(deadline);
                return None;
            }
            let now = Instant::now();
            match self.state {
                State::Off => {
                    pause_until(deadline);
                    return None;
                }
                State::Waiting { until } => {
                    if now >= until {
                        self.attach();
                    } else {
                        pause_until(until.min(deadline));
                    }
                }
                State::Streaming(_) => {
                    let slice = deadline.saturating_duration_since(now).min(HEALTH_INTERVAL);
                    match self.pump(slice) {
                        Ok(Some(lease)) => {
                            self.frames += 1;
                            return Some(lease);
                        }
                        Ok(None) => {}
                        Err(reason) => self.lose(reason),
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// One wait on the stream, with the health check when it is due.
    fn pump(&mut self, slice: Duration) -> Result<Option<FrameLease<usize>>, String> {
        let State::Streaming(live) = &mut self.state else {
            return Ok(None);
        };
        let now = Instant::now();
        if now.duration_since(live.checked) >= HEALTH_INTERVAL {
            live.checked = now;
            check(&self.client, &live.device)?;
        }
        match live.frames.next(slice) {
            Ok(lease) => Ok(lease),
            // The lease was refused and the frame given back. Lemnos's frame count keeps it.
            Err(styx_lemnos::Error::Held(_)) => Ok(None),
            Err(e) => Err(format!("frame stream: {e}")),
        }
    }

    /// Records a loss, detaches, and schedules the next attach.
    fn lose(&mut self, reason: String) {
        self.last_error = Some(reason);
        self.detach();
        self.schedule();
    }

    fn schedule(&mut self) {
        let delay = self.backoff.next_delay();
        self.retry_in = Some(delay);
        self.state = State::Waiting {
            until: Instant::now() + delay,
        };
    }

    /// Attaches now. Success streams; failure schedules the next try.
    fn attach(&mut self) {
        match self.try_attach() {
            Ok(live) => {
                self.attaches += 1;
                self.backoff.reset();
                self.last_error = None;
                self.retry_in = None;
                self.state = State::Streaming(Box::new(live));
            }
            Err(reason) => {
                self.last_error = Some(reason);
                self.schedule();
            }
        }
    }

    fn try_attach(&self) -> Result<Live, String> {
        let devices = self.client.list().map_err(|e| format!("list: {e}"))?;
        let entry =
            find_device(&devices, self.settings.device.as_deref()).map_err(|e| e.to_string())?;
        if entry.state != DeviceState::Running {
            return Err(format!(
                "{}: {:?}: {}",
                entry.name, entry.state, entry.reason
            ));
        }
        let name = entry.name.clone();
        let mut camera = self
            .client
            .open::<dyn CameraFrames>(&name)
            .map_err(|e| format!("{name}: open: {e}"))?;

        // Formats, then the choice. Compressed and unusable formats never reach the device.
        let count = camera
            .format_count()
            .map_err(|s| status_text(&format!("{name}: format_count"), s))?;
        let mut formats = Vec::with_capacity(usize::from(count));
        for index in 0..count {
            formats.push(
                camera
                    .format(index)
                    .map_err(|s| status_text(&format!("{name}: format {index}"), s))?,
            );
        }
        let Choice { index, format } = choose(&formats, &self.settings.preference)
            .ok_or_else(|| format!("{name}: no usable format among {count}"))?;

        // A camera left streaming (by an earlier attach the service lost) refuses a new format.
        let _ = camera.stop();
        camera
            .select_format(index)
            .map_err(|s| status_text(&format!("{name}: select_format {index}"), s))?;

        let mut stream_name = [0u8; MAX_STREAM_NAME];
        let n = camera
            .frame_stream(&mut stream_name)
            .map_err(|s| status_text(&format!("{name}: frame_stream"), s))?;
        let stream = std::str::from_utf8(&stream_name[..n])
            .map_err(|e| format!("{name}: stream name: {e}"))?;

        let code = FourCc::new(format.fourcc.to_le_bytes());
        let frames = styx_lemnos::open_frames(
            &self.client,
            entry.index,
            stream,
            code,
            format.width,
            format.height,
        )
        .map_err(|e| format!("{name}: frame stream {stream}: {e}"))?;

        if let Err(s) = camera.start() {
            let _ = self.client.close_stream(frames.into_stream());
            return Err(status_text(&format!("{name}: start"), s));
        }
        Ok(Live {
            device: name,
            camera,
            frames,
            format,
            checked: Instant::now(),
        })
    }

    /// Stops the device's stream and closes it. A device that is already gone cannot be stopped,
    /// and that is not an error.
    fn detach(&mut self) {
        if let State::Streaming(live) = std::mem::replace(&mut self.state, State::Off) {
            let Live {
                mut camera, frames, ..
            } = *live;
            let _ = camera.stop();
            let _ = self.client.close_stream(frames.into_stream());
        }
    }
}

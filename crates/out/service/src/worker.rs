//! One camera's thread: open it through styx-select, pump its frames, and when it fails wait and
//! try again, for as long as the service runs.
//!
//! Failure rule: a camera that leaves or fails comes back. Every failed attempt waits
//! `Backoff::delay(failures)` through the service's clock, which doubles from the config's first
//! wait and holds at the cap (5 to 10 s). A frame delivered since the last attempt resets the
//! count, so a camera that streams and then drops starts again at the first wait. Nothing gives
//! up on its own, except the configured `auto_off_after` policy, and a manual `stop` ends the
//! camera until a manual `start`.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};
use std::time::Duration;

use styx_controls::{Control, ControlValue, FrameMetadata, Update, WIRE_BYTES};
use styx_format::{FourCc, Interval};
use styx_lemnos::Client;
use styx_libcamera::CaptureConfig;
use styx_native::discover;
use styx_native_run::{AutoOff, Clock, Initial, Policy, Request, Selection, Session};
use styx_select::adapters::{LibcameraCapture, NativeCapture, V4l2Capture};
use styx_select::{Capture, CaptureError, Selected, Status};
use styx_socket::Producer;
use styx_v4l2::StreamConfig;

use crate::config::CameraConfig;
use crate::outlet::Outlet;
use crate::service::Shared;
use crate::slot::{Command, Health, KeepWindow, Publish, Slot, State};
use lemnos_client::DeviceHealth;

/// The client name the service gives the Lemnos daemon.
pub const CLIENT_NAME: &str = "styx-service";
/// How long a frame wait lasts. Commands are read between waits, so this bounds their latency.
const FRAME_WAIT: Duration = Duration::from_millis(100);
/// How long an idle, stopped or turned-off worker waits for a command before it looks again.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// Publishers made so far, in this process. A handshake plan is attached only to the publisher it
/// was made by.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The ring's history, in entries, for each camera's socket.
const HISTORY: u32 = 8;
/// The sensor's exposure when the config does not give one (native cameras).
const DEFAULT_EXPOSURE_US: u32 = 10_000;
/// The sensor's analogue gain when the config does not give one (native cameras).
const DEFAULT_GAIN_MILLI: u32 = 1_000;

/// The buffers a camera keeps to stream when the config does not say: half the pool's, at least
/// two, and never more than the pool has. The frame ring holds the rest for readers, at most
/// `HISTORY` of them.
pub fn default_reserve(buffers: usize) -> usize {
    (buffers / 2).max(2).min(buffers)
}

/// The keep window for a pool of `buffers` with `reserve` kept for the camera: the newest
/// `min(HISTORY, buffers - reserve)` frames stay with the ring, and at least one does.
pub fn keep_for(buffers: usize, reserve: usize) -> usize {
    (HISTORY as usize)
        .min(buffers.saturating_sub(reserve))
        .max(1)
}

/// How one attempt at a camera ended.
#[derive(Debug)]
pub(crate) enum Ended {
    /// A manual stop, or shutdown.
    Stopped,
    /// The camera failed. `delivered` is whether any frame came from this attempt.
    Fault { text: String, delivered: bool },
}

/// The worker loop for one camera. Runs until shutdown.
pub(crate) fn run(slot: &Slot, commands: &Receiver<Command>, shared: &Shared) {
    let mut outlet = shared.outlets.attach(&slot.id);
    let mut failures: u32 = 0;
    let mut paused: Option<State> = None;
    while !shared.shutdown.load(Ordering::Acquire) {
        if let Some(state) = paused {
            match commands.recv_timeout(IDLE_WAIT) {
                Ok(Command::Start) => {
                    paused = None;
                    failures = 0;
                    slot.reset_failures();
                }
                Ok(Command::Stop) => {}
                Ok(Command::Set { reply, .. }) => {
                    let _ = reply.send(Err(format!("camera is {state}, not streaming")));
                }
                // Not publishing: the reader is closed and connects again later.
                Ok(Command::Handshake(reply)) => {
                    let _ = reply.send(None);
                }
                Ok(Command::Attach(..)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        // Commands that came in while the camera was waiting.
        slot.wake.store(false, Ordering::Release);
        match drain(commands, slot) {
            Drained::Idle => {}
            Drained::Stop => {
                paused = Some(State::Stopped);
                slot.set_state(State::Stopped);
                continue;
            }
            Drained::Gone => return,
        }

        // Not listed by any scan: nothing to open, so nothing has failed. The scan lists the camera
        // when it comes back, and this worker picks it up here. Commands are still read.
        let Some(selection) = slot.selected() else {
            slot.set_state(State::Absent);
            match commands.recv_timeout(IDLE_WAIT) {
                Ok(Command::Stop) => {
                    paused = Some(State::Stopped);
                    slot.set_state(State::Stopped);
                }
                Ok(Command::Start) => {}
                Ok(Command::Set { reply, .. }) => {
                    let _ = reply.send(Err("camera is absent, not streaming".to_string()));
                }
                Ok(Command::Handshake(reply)) => {
                    let _ = reply.send(None);
                }
                Ok(Command::Attach(..)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        };
        slot.set_state(State::Backoff);
        let ended = attempt(slot, commands, shared, outlet.as_mut(), &selection);
        match ended {
            Ended::Stopped => {
                paused = Some(State::Stopped);
                slot.set_state(State::Stopped);
            }
            Ended::Fault { text, delivered } => {
                if delivered {
                    failures = 0;
                }
                failures = failures.saturating_add(1);
                slot.failed(text, failures);
                if shared
                    .auto_off
                    .is_some_and(|limit: NonZeroU32| failures >= limit.get())
                {
                    paused = Some(State::Off);
                    slot.set_state(State::Off);
                    continue;
                }
                let delay = shared.backoff.delay(failures);
                slot.backing_off(delay);
                slot.wake.store(false, Ordering::Release);
                match drain(commands, slot) {
                    Drained::Idle => {}
                    Drained::Stop => {
                        paused = Some(State::Stopped);
                        slot.set_state(State::Stopped);
                        continue;
                    }
                    Drained::Gone => return,
                }
                shared.clock.wait(delay, &slot.wake);
            }
        }
    }
}

enum Drained {
    Idle,
    Stop,
    Gone,
}

/// Reads the commands waiting now. A `Set` with no camera running is refused; `Start` has no
/// effect on a running camera.
fn drain(commands: &Receiver<Command>, slot: &Slot) -> Drained {
    loop {
        match commands.try_recv() {
            Ok(Command::Stop) => return Drained::Stop,
            Ok(Command::Start) => {}
            Ok(Command::Set { reply, .. }) => {
                let _ = reply.send(Err(format!(
                    "camera is {}, not streaming",
                    slot.snapshot().state
                )));
            }
            Ok(Command::Handshake(reply)) => {
                let _ = reply.send(None);
            }
            Ok(Command::Attach(..)) => {}
            Err(TryRecvError::Empty) => return Drained::Idle,
            Err(TryRecvError::Disconnected) => return Drained::Gone,
        }
    }
}

/// One attempt: open the camera, pump it until it ends.
fn attempt(
    slot: &Slot,
    commands: &Receiver<Command>,
    shared: &Shared,
    outlet: &mut dyn Outlet,
    selection: &Selected,
) -> Ended {
    let opened = open(selection, slot, shared, |capture, selections| {
        pump(capture, slot, commands, shared, outlet, selections)
    });
    match opened {
        Ok(ended) => ended,
        Err(text) => Ended::Fault {
            text,
            delivered: false,
        },
    }
}

/// Opens `selection` with its owner's capture and runs `run` on it. The capture (and the Lemnos
/// client and camera it borrows) live for the call. Any error is text for the status.
///
/// `run` gets the mode choices a native camera makes as it starts (none for the other backends).
fn open<R>(
    selection: &Selected,
    slot: &Slot,
    shared: &Shared,
    run: impl FnOnce(&mut dyn Capture, Option<&Receiver<Selection>>) -> R,
) -> Result<R, String> {
    use styx_select::Backend;
    let settings = &slot.settings;
    match selection.owner {
        Backend::Native => {
            let socket = shared
                .lemnos_socket
                .as_ref()
                .ok_or("a native camera needs lemnos_socket in the config")?;
            let client =
                Client::connect(socket, CLIENT_NAME).map_err(|e| format!("lemnos: {e}"))?;
            let found = discover(&client).map_err(|e| format!("lemnos discovery: {e}"))?;
            let camera = found
                .cameras
                .iter()
                .find(|c| c.name() == selection.key)
                .ok_or_else(|| format!("native camera {} is not on the daemon", selection.key))?;
            slot.set_health(native_health(camera.health(), camera.reason()));
            // The rate is the request as given. A rate above the chosen mode's ceiling is answered
            // with the ceiling, and the status says so (the mode choice). The config must give one.
            let fps = settings.fps_milli.ok_or_else(|| {
                format!(
                    "native camera {} needs fps_milli in the config",
                    selection.id
                )
            })?;
            let format = settings
                .format
                .as_deref()
                .map(|text| {
                    text.parse::<FourCc>().map_err(|_| {
                        format!(
                            "camera {}: format {text:?} is not a four-character code",
                            selection.id
                        )
                    })
                })
                .transpose()?;
            let request = Request {
                width: settings.width.unwrap_or(0),
                height: settings.height.unwrap_or(0),
                fps_milli: fps,
                format,
                bit_depth: settings.bit_depth,
                bayer: None,
            };
            let initial = Initial {
                exposure_us: settings.exposure_us.unwrap_or(DEFAULT_EXPOSURE_US),
                gain_milli: settings.gain_milli.unwrap_or(DEFAULT_GAIN_MILLI),
                frame_rate_milli: fps,
            };
            let clock: &dyn Clock = &*shared.clock;
            let (choices, chosen) = channel();
            let session = Session::new(&client, camera, request, native_policy(shared), clock)
                .with_controls(initial)
                .with_selection_sink(choices);
            let mut capture = NativeCapture::start(session).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, Some(&chosen)))
        }
        Backend::Libcamera => {
            let code = format_of(selection, settings)?;
            let (width, height) = size_of(selection, settings)?;
            let config = CaptureConfig {
                code,
                width,
                height,
                buffers: settings.buffers.unwrap_or(4),
            };
            let mut capture =
                LibcameraCapture::open(&selection.key, config).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, None))
        }
        Backend::V4l2 => {
            let code = format_of(selection, settings)?;
            let (width, height) = size_of(selection, settings)?;
            let mut config = StreamConfig::new(code, width, height);
            if let Some(buffers) = settings.buffers {
                config.buffers = buffers;
            }
            config.interval = settings.fps_milli.and_then(|fps| Interval::new(1_000, fps));
            let mut capture =
                V4l2Capture::start(&selection.key, config).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, None))
        }
    }
}

/// The health a native camera reports, as the service shows it.
fn native_health(health: DeviceHealth, reason: &str) -> Health {
    let reason = if reason.is_empty() {
        "no reason given".to_string()
    } else {
        reason.to_string()
    };
    match health {
        DeviceHealth::Ok => Health::Healthy,
        DeviceHealth::Degraded => Health::Degraded(reason),
        DeviceHealth::Failed => Health::Failed(reason),
    }
}

fn native_policy(shared: &Shared) -> Policy {
    Policy {
        backoff: shared.backoff,
        // A native session never waits for itself: its first fault turns it off, and this
        // worker waits (through the service's clock) and opens the camera again.
        auto_off: AutoOff::AfterFailures(NonZeroU32::MIN),
        frame_timeout: FRAME_WAIT,
        stall_waits: 4,
    }
}

fn format_of(selection: &Selected, settings: &CameraConfig) -> Result<FourCc, String> {
    let text = settings.format.as_deref().ok_or_else(|| {
        format!(
            "camera {} needs format in the config for {}",
            selection.id, selection.owner
        )
    })?;
    text.parse::<FourCc>().map_err(|_| {
        format!(
            "camera {}: format {text:?} is not a four-character code",
            selection.id
        )
    })
}

fn size_of(selection: &Selected, settings: &CameraConfig) -> Result<(u32, u32), String> {
    match (settings.width, settings.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(format!(
            "camera {} needs width and height in the config for {}",
            selection.id, selection.owner
        )),
    }
}

/// Records a frame-rate control the camera accepted as the rate asked for, so the status shows it.
fn note_requested(slot: &Slot, updates: &[Update]) {
    for update in updates {
        if let (Control::FrameRate, ControlValue::Manual(milli)) = (update.control, update.value) {
            slot.set_requested(Some(milli));
        }
    }
}

/// Runs frames through the outlet, the socket and the status until the camera ends, and answers
/// commands between frames.
///
/// The camera's pool is the one its first frame came from ([`FrameLease::pool`]): a producer over
/// it publishes each frame's buffer as it is, with the frame's metadata as the opaque part. A
/// camera whose frames are not fd-backed cannot be published without a copy; it says so in the
/// status and publishes nothing. A reader that connects before the first frame is closed (it
/// connects again). The producer, and so every reader's connection, ends with this attempt.
fn pump(
    capture: &mut dyn Capture,
    slot: &Slot,
    commands: &Receiver<Command>,
    shared: &Shared,
    outlet: &mut dyn Outlet,
    selections: Option<&Receiver<Selection>>,
) -> Ended {
    slot.started();
    let mut delivered = false;
    let mut publisher: Option<Producer<FrameMetadata>> = None;
    let mut generation = 0_u64;
    let mut unpublishable = false;
    let mut window = KeepWindow::default();
    let ended = loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break Ended::Stopped;
        }
        // The mode each start chose (a native camera restarts inside its session on a fault).
        if let Some(chosen) = selections {
            for mode in chosen.try_iter() {
                slot.set_mode(mode);
            }
        }
        match capture.next_frame(FRAME_WAIT) {
            Ok(Some(lease)) => {
                delivered = true;
                if publisher.is_none() && !unpublishable {
                    let buffers = lease.pool().buffers();
                    match Producer::with_pool(lease.pool(), HISTORY) {
                        Ok(mut producer) => {
                            let reserve = slot
                                .settings
                                .hardware_reserve
                                .map_or(default_reserve(buffers), |r| (r as usize).min(buffers));
                            let keep = producer.set_keep(keep_for(buffers, reserve));
                            window = KeepWindow {
                                keep,
                                buffers,
                                hardware_reserve: reserve,
                                reclaimed: 0,
                            };
                            slot.set_keep(window);
                            generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
                            publisher = Some(producer);
                            slot.set_publish(Publish::Publishing);
                        }
                        Err(error) => {
                            unpublishable = true;
                            slot.set_publish(Publish::NotPublishable(error.to_string()));
                        }
                    }
                }
                slot.frame(&lease);
                outlet.frame(&slot.id, &lease);
                if let Some(producer) = publisher.as_mut() {
                    let mut blob = [0_u8; WIRE_BYTES];
                    let len = lease.meta().to_bytes(&mut blob);
                    let dropped = producer.publish_with(lease, &blob[..len]).is_err();
                    slot.published(dropped);
                    producer.reclaim();
                    let stats = producer.stats();
                    slot.set_readers(stats.readers);
                    if window.reclaimed != stats.reclaimed {
                        window.reclaimed = stats.reclaimed;
                        slot.set_keep(window);
                    }
                }
                slot.set_rate(capture.rate());
            }
            Ok(None) => {}
            Err(error) => {
                break Ended::Fault {
                    text: describe(&error),
                    delivered,
                };
            }
        }
        match capture.status() {
            Status::Streaming => slot.resumed(),
            Status::Retrying { reason } => slot.retrying(reason),
            Status::Stopped => {
                break Ended::Fault {
                    text: "the backend reports the camera stopped".to_string(),
                    delivered,
                };
            }
            Status::Failed { reason } => {
                break Ended::Fault {
                    text: reason,
                    delivered,
                };
            }
        }
        let mut stop = None;
        loop {
            match commands.try_recv() {
                Ok(Command::Stop) => {
                    stop = Some(Ended::Stopped);
                    break;
                }
                Ok(Command::Start) => {}
                Ok(Command::Set { updates, reply }) => {
                    let result = set(capture, &updates);
                    if result.is_ok() {
                        note_requested(slot, &updates);
                    }
                    let _ = reply.send(result);
                }
                Ok(Command::Handshake(reply)) => {
                    // The plan is made here, on the producer's thread; it is run elsewhere.
                    let plan = publisher
                        .as_ref()
                        .and_then(|producer| producer.handshake_plan().ok());
                    let _ = reply.send(plan.map(|plan| (generation, plan)));
                }
                Ok(Command::Attach(planned, accepted)) => {
                    // Attached between frames: a table entry and a doorbell, nothing more.
                    if let Some(producer) = publisher.as_mut()
                        && planned == generation
                        && producer.attach(accepted).is_ok()
                    {
                        slot.set_readers(producer.stats().readers);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stop = Some(Ended::Stopped);
                    break;
                }
            }
        }
        if let Some(ended) = stop {
            break ended;
        }
    };
    // The producer goes first: its readers see their connection end, and the buffers it held go
    // back before the capture is stopped.
    if publisher.take().is_some() {
        slot.set_readers(0);
        slot.set_publish(Publish::Waiting);
    }
    ended
}

fn set(capture: &mut dyn Capture, updates: &[Update]) -> Result<(), String> {
    capture.set_controls(updates).map_err(|e| describe(&e))
}

fn describe(error: &CaptureError) -> String {
    match error {
        CaptureError::Stopped => "the camera stopped".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_cameras_health_and_reason_are_shown_and_a_degraded_one_is_usable() {
        assert_eq!(native_health(DeviceHealth::Ok, "x"), Health::Healthy);
        assert_eq!(
            native_health(DeviceHealth::Degraded, "link not confirmed"),
            Health::Degraded("link not confirmed".to_string())
        );
        assert_eq!(
            native_health(DeviceHealth::Failed, ""),
            Health::Failed("no reason given".to_string())
        );
    }
}

#[cfg(test)]
mod keep_tests {
    use super::*;

    #[test]
    fn the_default_reserve_is_half_the_pool_and_at_least_two() {
        assert_eq!(default_reserve(8), 4);
        assert_eq!(default_reserve(5), 2);
        assert_eq!(default_reserve(2), 2);
        assert_eq!(default_reserve(1), 1);
    }

    #[test]
    fn the_keep_window_is_the_history_or_what_the_camera_does_not_need_and_at_least_one() {
        // 8 buffers, reserve 4: four frames for readers, below the history of 8.
        assert_eq!(keep_for(8, 4), 4);
        // 16 buffers, reserve 4: the history (8) is the limit.
        assert_eq!(keep_for(16, 4), 8);
        // The camera needs all of them: one frame is still kept.
        assert_eq!(keep_for(4, 4), 1);
    }
}

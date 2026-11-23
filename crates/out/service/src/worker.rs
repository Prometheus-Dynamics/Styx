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
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::time::Duration;

use styx_controls::{Control, ControlValue, FrameMetadata, Update, WIRE_BYTES};
use styx_format::{FourCc, Interval};
use styx_lemnos::Carried;
use styx_lemnos::Client;
use styx_libcamera::CaptureConfig;
use styx_native::{Camera, Refusal, discover};
use styx_native_run::{
    AutoOff, Clock, ExposureReport, Initial, Policy, Request, Selection, Session,
};
use styx_select::adapters::{LibcameraCapture, NativeCapture, V4l2Capture};
use styx_select::{Capture, CaptureError, Edge, Selected, Status};
use styx_socket::{Accepted, HandshakePlan, Producer, StreamEnd};
use styx_v4l2::StreamConfig;

use crate::config::CameraConfig;
use crate::outlet::Outlet;
use crate::service::Shared;
use crate::slot::{Command, FAULT_DEVICE_FAILED, Health, KeepWindow, Publish, Slot, State};
use lemnos_client::DeviceHealth;
use lemnos_contract_camera_receiver::CameraReceiver;
use lemnos_contract_camera_sensor::CameraSensor;
use lemnos_core::Status as LemnosStatus;

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

/// A new publisher generation, unique in this process: a reader's plan names the producer it was
/// made by.
pub(crate) fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// The camera's producer, held across attempts. While the camera retries it stays attached, so its
/// readers stay linked and are told of the stall. The pool changes when the camera reopens (its
/// buffers are made at open), so the held producer ends (`Closed`) at the first frame of the next
/// attempt and a new one takes its place: readers reconnect then. Reusing it would need the same
/// buffers, which a reopen never gives.
struct Held {
    producer: Producer<FrameMetadata>,
    generation: u64,
}

type Publisher = Option<Held>;

/// Ends the held producer, if any. Its readers see the end (`Closed` unless `end` says more: a
/// device switched off for good says `DeviceFailed`), and the camera's publishing is reset.
fn release(publisher: &mut Publisher, slot: &Slot, end: Option<StreamEnd>) {
    if let Some(mut held) = publisher.take() {
        if let Some(end) = end {
            held.producer.end(end);
        }
        slot.set_readers(0);
        slot.set_publish(Publish::Waiting);
        slot.clear_stall();
    }
}

/// Answers a reader's handshake request from the held producer. `None` when none is publishing.
fn answer_handshake(publisher: &Publisher, reply: Sender<Option<(u64, HandshakePlan)>>) {
    let answer = publisher.as_ref().and_then(|held| {
        held.producer
            .handshake_plan()
            .ok()
            .map(|plan| (held.generation, plan))
    });
    let _ = reply.send(answer);
}

/// Attaches an answered reader to the producer it was planned on. Any other generation is closed.
fn attach_reader(publisher: &mut Publisher, planned: u64, accepted: Accepted, slot: &Slot) {
    if let Some(held) = publisher.as_mut()
        && planned == held.generation
        && held.producer.attach(accepted).is_ok()
    {
        slot.set_readers(held.producer.stats().readers);
    }
}
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

/// The reserve to set for a camera whose reader declared `hold` buffers at once: at least what
/// the hold leaves the camera, plus one for the frame in hand while the ring and its readers keep
/// the rest, so the next frame can always be taken. With no hold declared, `asked` as it is.
pub fn reserve_for(buffers: usize, asked: usize, hold: Option<u32>) -> usize {
    match hold {
        Some(hold) => asked
            .max(buffers.saturating_sub(hold as usize) + 1)
            .min(buffers),
        None => asked,
    }
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
    let mut publisher: Publisher = None;
    while !shared.shutdown.load(Ordering::Acquire) {
        if let Some(state) = paused {
            match commands.recv_timeout(IDLE_WAIT) {
                Ok(Command::Start) => {
                    paused = None;
                    failures = 0;
                    slot.reset_failures();
                }
                Ok(Command::Stop) => {}
                Ok(
                    Command::Set { reply, .. }
                    | Command::SetDevice { reply, .. }
                    | Command::SetDeviceReal { reply, .. },
                ) => {
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
        match drain(commands, slot, &mut publisher) {
            Drained::Idle => {}
            Drained::Stop => {
                release(&mut publisher, slot, None);
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
                Ok(
                    Command::Set { reply, .. }
                    | Command::SetDevice { reply, .. }
                    | Command::SetDeviceReal { reply, .. },
                ) => {
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
        let ended = attempt(
            slot,
            commands,
            shared,
            outlet.as_mut(),
            &selection,
            &mut publisher,
        );
        match ended {
            Ended::Stopped => {
                release(&mut publisher, slot, None);
                paused = Some(State::Stopped);
                slot.set_state(State::Stopped);
            }
            Ended::Fault { text, delivered } => {
                if delivered {
                    failures = 0;
                }
                failures = failures.saturating_add(1);
                if shared
                    .auto_off
                    .is_some_and(|limit: NonZeroU32| failures >= limit.get())
                {
                    // Switched off for good by its own policy: its readers learn the device failed.
                    release(&mut publisher, slot, Some(StreamEnd::DeviceFailed));
                    slot.failed(text, failures);
                    slot.set_fault(FAULT_DEVICE_FAILED);
                    paused = Some(State::Off);
                    slot.set_state(State::Off);
                    continue;
                }
                // The camera retries: the producer stays attached through the backoff, and its
                // readers are told of the stall. This stall is the service's own: the capture has
                // ended and its devices are halted, so no Lemnos stream is left to report one. A
                // stall the hardware reports while the stream is attached (`Next::Stalled` and
                // `Next::Resumed`) reaches the readers through the capture's edges instead
                // (`apply_edge`).
                let since = styx_socket::clock_ns();
                if let Some(held) = publisher.as_mut() {
                    held.producer.stall(since);
                    slot.set_readers(held.producer.stats().readers);
                }
                slot.stalled(since);
                slot.failed(
                    format!("camera failed, retrying since {since}: {text}"),
                    failures,
                );
                let delay = shared.backoff.delay(failures);
                slot.backing_off(delay);
                slot.wake.store(false, Ordering::Release);
                match drain(commands, slot, &mut publisher) {
                    Drained::Idle => {}
                    Drained::Stop => {
                        release(&mut publisher, slot, None);
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
fn drain(commands: &Receiver<Command>, slot: &Slot, publisher: &mut Publisher) -> Drained {
    loop {
        match commands.try_recv() {
            Ok(Command::Stop) => return Drained::Stop,
            Ok(Command::Start) => {}
            Ok(
                Command::Set { reply, .. }
                | Command::SetDevice { reply, .. }
                | Command::SetDeviceReal { reply, .. },
            ) => {
                let _ = reply.send(Err(format!(
                    "camera is {}, not streaming",
                    slot.snapshot().state
                )));
            }
            // A reader asking while the camera retries joins the producer that stays attached.
            Ok(Command::Handshake(reply)) => answer_handshake(publisher, reply),
            Ok(Command::Attach(planned, accepted)) => {
                attach_reader(publisher, planned, accepted, slot)
            }
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
    publisher: &mut Publisher,
) -> Ended {
    let opened = open(selection, slot, shared, |capture, reports| {
        pump(capture, slot, commands, shared, outlet, reports, publisher)
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
    run: impl FnOnce(&mut dyn Capture, Option<&Reports>) -> R,
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
            // A camera whose device contract is another build's is refused at discovery: its status
            // names the contract and the sentence, and the worker backs off like any failure.
            if let Some((_, Refusal::Contract { contract, text, .. })) =
                found.refused.iter().find(|entry| entry.0 == selection.key)
            {
                slot.set_contract(Some(contract.clone()));
                return Err(text.clone());
            }
            slot.set_contract(None);
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
            let (exposure_sink, exposures) = channel();
            let session = || {
                Session::new(&client, camera, request, native_policy(shared), clock)
                    .with_controls(initial)
                    .with_selection_sink(choices.clone())
                    .with_exposure_sink(exposure_sink.clone())
            };
            // A device contract another build's, found as the session opens its devices: the same
            // status and fault code as discovery's refusal (one meaning, one form).
            let refused = |e: CaptureError| {
                if let CaptureError::Contract { contract, .. } = &e {
                    slot.set_contract(Some(contract.clone()));
                }
                e.to_string()
            };
            let mut capture = match NativeCapture::start(session()) {
                Ok(capture) => capture,
                Err(e) if left_started(&e.to_string()) => {
                    // A previous owner left the sensor streaming, so it refuses a mode. Stop
                    // the sensor and the receiver once through the contract, then try again
                    // once. A second refusal is an ordinary failure: it backs off like any other.
                    let note = stop_left_started(&client, camera);
                    slot.recovered(note.clone());
                    eprintln!("styx-service: camera {}: {note}", slot.id);
                    NativeCapture::start(session()).map_err(refused)?
                }
                Err(e) => return Err(refused(e)),
            };
            let reports = Reports {
                modes: chosen,
                exposures,
                isps: camera.isps().to_vec(),
            };
            Ok(run(&mut capture, Some(&reports)))
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

/// Whether a session's start failed because the sensor refused a mode as busy: the sensor is
/// streaming, which is what a previous owner that did not stop it leaves behind. The text is the
/// session's, which formats a refused call's status with `Debug`.
fn left_started(text: &str) -> bool {
    text.contains(&format!(
        "sensor.select_mode refused: {:?}",
        LemnosStatus::BUSY
    ))
}

/// Stops the camera's receiver, then its sensor, through the Lemnos contract, as a manual stop
/// would (the order and why: `halt` in styx-native-run's session). Each is tried even when the
/// other refuses. The text says what was done, for the status.
fn stop_left_started(client: &Client, camera: &Camera) -> String {
    let mut problems = Vec::new();
    let sensor = &camera.sensor().name;
    let receiver = &camera.receiver().name;
    match client.open::<dyn CameraReceiver>(receiver) {
        Ok(mut device) => {
            if let Err(status) = device.stop() {
                problems.push(format!("receiver.stop refused: {status:?}"));
            }
        }
        Err(e) => problems.push(format!("open {receiver}: {e}")),
    }
    match client.open::<dyn CameraSensor>(sensor) {
        Ok(mut device) => {
            if let Err(status) = device.stop() {
                problems.push(format!("sensor.stop refused: {status:?}"));
            }
        }
        Err(e) => problems.push(format!("open {sensor}: {e}")),
    }
    if problems.is_empty() {
        format!("sensor {sensor} was left started: stopped it and receiver {receiver}, retrying")
    } else {
        format!(
            "sensor {sensor} was left started: {}; retrying",
            problems.join("; ")
        )
    }
}

fn native_policy(shared: &Shared) -> Policy {
    Policy {
        backoff: shared.backoff,
        // A native session never waits for itself: its first fault turns it off, and this
        // worker waits (through the service's clock) and opens the camera again.
        auto_off: AutoOff::AfterFailures(NonZeroU32::MIN),
        frame_timeout: FRAME_WAIT,
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

/// What a native session reports while it runs: the mode each start chose, and the exposure
/// (asked, applied, and the clamp with its reason).
pub(crate) struct Reports {
    modes: Receiver<Selection>,
    exposures: Receiver<ExposureReport>,
    /// The camera's ISP devices, in link order, for the processed stream.
    isps: Vec<styx_lemnos::DeviceEntry>,
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
///
/// With a processed stream asked for (`isp`), the camera's ISP thread runs beside this loop for the
/// attempt and is offered each frame (see `isp`); it ends with the attempt.
fn pump(
    capture: &mut dyn Capture,
    slot: &Slot,
    commands: &Receiver<Command>,
    shared: &Shared,
    outlet: &mut dyn Outlet,
    reports: Option<&Reports>,
    publisher: &mut Publisher,
) -> Ended {
    // The config was checked at start: a plan that does not parse here is no processed stream.
    let Ok(Some(plan)) = slot.settings.isp_plan() else {
        return pump_frames(
            capture, slot, commands, shared, outlet, reports, publisher, None,
        );
    };
    let (feed, frames) = crate::isp::handoff();
    let clock = &shared.clock;
    let now = move || clock.now();
    let ctx = crate::isp::Context {
        plan,
        slot,
        lemnos_socket: shared.lemnos_socket.clone(),
        isps: reports.map(|r| r.isps.clone()).unwrap_or_default(),
        now: &now,
        tuning_dirs: &shared.config.tuning_dirs,
    };
    let ctx = &ctx;
    std::thread::scope(|scope| {
        let isp = std::thread::Builder::new()
            .name(format!("styx-isp:{}", slot.id))
            .spawn_scoped(scope, move || crate::isp::run(ctx, &frames));
        if let Err(e) = &isp {
            slot.isp_down(&format!("isp thread: {e}"), styx_socket::clock_ns());
        }
        let ended = pump_frames(
            capture,
            slot,
            commands,
            shared,
            outlet,
            reports,
            publisher,
            isp.is_ok().then_some(&feed),
        );
        // The feed goes first, so the ISP thread ends and gives its raw frames back before the
        // capture stops.
        drop(feed);
        ended
    })
}

#[allow(clippy::too_many_arguments)]
fn pump_frames(
    capture: &mut dyn Capture,
    slot: &Slot,
    commands: &Receiver<Command>,
    shared: &Shared,
    outlet: &mut dyn Outlet,
    reports: Option<&Reports>,
    publisher: &mut Publisher,
    isp: Option<&crate::isp::Feed>,
) -> Ended {
    slot.started();
    // The camera opened (again): its controls are read now, and the count moves.
    let mut controls_generation = capture.controls_generation();
    slot.set_controls_info(capture.controls_info());
    let mut delivered = false;
    // A producer held over from an earlier attempt belongs to the buffers that attempt had; the
    // first frame of this one ends it and publishes from a new pool.
    let mut replacing = publisher.is_some();
    let mut unpublishable = false;
    let mut window = KeepWindow::default();
    // The 3A loop runs on the ISP thread; its sensor requests are posted here, between frames.
    let aaa = isp.is_some() && slot.settings.aaa == Some(true);
    let ended = loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break Ended::Stopped;
        }
        // The mode each start chose (a native camera restarts inside its session on a fault).
        // The exposure in force, as the native session reports it when it changes.
        if let Some(reports) = reports {
            for mode in reports.modes.try_iter() {
                slot.set_mode(mode);
            }
            for exposure in reports.exposures.try_iter() {
                slot.set_exposure(exposure);
            }
        }
        let next = capture.next_frame(FRAME_WAIT);
        slot.set_lemnos_counts(capture.lemnos_skipped(), capture.lemnos_at_limit());
        match next {
            Ok(Some(lease)) => {
                delivered = true;
                if replacing {
                    replacing = false;
                    release(publisher, slot, None);
                    slot.camera_back();
                }
                if publisher.is_none() && !unpublishable {
                    let buffers = lease.pool().buffers();
                    match Producer::with_pool(lease.pool(), HISTORY) {
                        Ok(mut producer) => {
                            // The reserve first: the keep window is clamped to what it leaves.
                            let asked = slot
                                .settings
                                .hardware_reserve
                                .map_or(default_reserve(buffers), |r| (r as usize).min(buffers));
                            let base = reserve_for(buffers, asked, capture.hold());
                            // The ISP's raw frames come out of the pool too: its share is
                            // added to the reserve, or `isp` is refused when it would leave raw
                            // readers too few frames.
                            let with_isp =
                                isp.map(|feed| match crate::isp::hold_for(buffers, base) {
                                    Ok(hold) => {
                                        feed.set_depth(hold.depth);
                                        hold.reserve
                                    }
                                    Err(why) => {
                                        feed.refuse(why);
                                        base
                                    }
                                });
                            let reserve = producer.set_reserve(with_isp.unwrap_or(base));
                            let keep = producer.set_keep(keep_for(buffers, reserve));
                            window = KeepWindow {
                                keep,
                                buffers,
                                hardware_reserve: reserve,
                                reclaimed: 0,
                                reserve_cuts: 0,
                                kept: keep,
                                bound_unmet: 0,
                            };
                            slot.set_keep(window);
                            let generation = next_generation();
                            *publisher = Some(Held {
                                producer,
                                generation,
                            });
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
                if let Some(isp) = isp {
                    isp.offer(&lease, slot);
                }
                if let Some(held) = publisher.as_mut() {
                    let producer = &mut held.producer;
                    let mut blob = [0_u8; WIRE_BYTES];
                    let len = lease.meta().to_bytes(&mut blob);
                    let dropped = producer.publish_with(lease, &blob[..len]).is_err();
                    slot.published(dropped);
                    producer.reclaim();
                    let stats = producer.stats();
                    slot.set_readers(stats.readers);
                    if window.reclaimed != stats.reclaimed
                        || window.reserve_cuts != stats.cut_off_for_reserve
                        || window.kept != stats.kept
                        || window.bound_unmet != stats.bound_unmet
                    {
                        window.reclaimed = stats.reclaimed;
                        window.reserve_cuts = stats.cut_off_for_reserve;
                        window.kept = stats.kept;
                        window.bound_unmet = stats.bound_unmet;
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
        if aaa {
            post_aaa(capture, slot);
        }
        if let Some(edge) = capture.take_edge() {
            apply_edge(publisher, slot, edge);
        }
        // A range that moved (a frame rate changing exposure's ceiling, a restart): read again.
        let generation = capture.controls_generation();
        if generation != controls_generation {
            controls_generation = generation;
            slot.set_controls_info(capture.controls_info());
        }
        match capture.status() {
            Status::Streaming => {
                slot.layout_refused(None);
                slot.set_contract(None);
                slot.resumed();
            }
            Status::Retrying {
                reason,
                layout,
                contract,
            } => {
                slot.layout_refused(layout);
                slot.set_contract(contract);
                slot.retrying(reason);
            }
            Status::Stopped => {
                break Ended::Fault {
                    text: "the backend reports the camera stopped".to_string(),
                    delivered,
                };
            }
            Status::Failed {
                reason,
                layout,
                contract,
            } => {
                slot.layout_refused(layout);
                slot.set_contract(contract);
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
                        controls_moved(capture, slot, &mut controls_generation);
                    }
                    let _ = reply.send(result);
                }
                Ok(Command::SetDevice { id, value, reply }) => {
                    let result = capture.set_device(id, value).map_err(|e| describe(&e));
                    if result.is_ok() {
                        controls_moved(capture, slot, &mut controls_generation);
                    }
                    let _ = reply.send(result);
                }
                Ok(Command::SetDeviceReal { id, value, reply }) => {
                    let result = capture.set_device_real(id, value).map_err(|e| describe(&e));
                    if result.is_ok() {
                        controls_moved(capture, slot, &mut controls_generation);
                    }
                    let _ = reply.send(result);
                }
                // The plan is made here, on the producer's thread; it is run elsewhere.
                Ok(Command::Handshake(reply)) => answer_handshake(publisher, reply),
                // Attached between frames: a table entry and a doorbell, nothing more.
                Ok(Command::Attach(planned, accepted)) => {
                    attach_reader(publisher, planned, accepted, slot);
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
    // A stopped camera's producer goes first: its readers see their connection end, and the
    // buffers it held go back before the capture is stopped. A failed one is kept by the caller,
    // stalled, through the backoff.
    if matches!(ended, Ended::Stopped) {
        release(publisher, slot, None);
    }
    ended
}

/// Applies a camera's stall or return to the producer its readers are on. A stall is health: the
/// producer stays attached and its readers are told `Retrying` since the failure, with the camera's
/// own reason. A return ends it: the readers are told the camera runs. A return that lost what it
/// was given restarts the frame sequence: the slot starts counting from its first frame again, so
/// no gap is counted for it.
fn apply_edge(publisher: &mut Publisher, slot: &Slot, edge: Edge) {
    match edge {
        Edge::Stalled { since_ns, text } => {
            // A silence the session names has no time of its own (0): it starts now.
            let since_ns = if since_ns == 0 {
                styx_socket::clock_ns()
            } else {
                since_ns
            };
            if let Some(held) = publisher.as_mut() {
                held.producer.stall(since_ns);
                slot.set_readers(held.producer.stats().readers);
            }
            slot.stalled(since_ns);
            slot.stall_health(&text);
            slot.retrying(text);
        }
        Edge::Resumed { carried } => {
            let kept = carried == Carried::Kept;
            if let Some(held) = publisher.as_mut() {
                let carried = if kept {
                    styx_socket::Carried::Kept
                } else {
                    styx_socket::Carried::Lost
                };
                held.producer.resume(carried);
                slot.set_readers(held.producer.stats().readers);
            }
            slot.clear_stall();
            slot.return_health();
            slot.camera_back();
            slot.resumed();
            if !kept {
                slot.discontinuity();
            }
        }
    }
}

/// The 3A loop's side of the capture thread: the sensor's ranges and delays for the loop, and the
/// loop's newest exposure and gain posted for the frame they name (a manual value in place of the
/// loop's from the frame it was set for: see `aaa::Link::take_request`). A request the backend
/// refuses is the loop's reason in the status; the next one is posted as usual.
fn post_aaa(capture: &mut dyn Capture, slot: &Slot) {
    slot.aaa.set_timing(capture.sensor_timing());
    if let Some(updates) = slot.aaa.take_request()
        && let Err(e) = capture.set_controls(&updates)
    {
        slot.aaa_refused(&describe(&e));
    }
}

/// A set was applied: the controls count moves once, with the backend's new report when it has one.
fn controls_moved(capture: &dyn Capture, slot: &Slot, seen: &mut u64) {
    let generation = capture.controls_generation();
    if generation == *seen {
        slot.controls_changed();
    } else {
        *seen = generation;
        slot.set_controls_info(capture.controls_info());
    }
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

//! B: A + the sensor and the camera runtime. The OV9782's compiled description (postcard bytes
//! made at build time, no TOML parser in the image), its driver over a register bus, the
//! frame-exact control schedule served at frame starts, embedded data reported per frame,
//! `styx-runtime`'s `Camera` streaming raw frames, which go to a consumer as `FrameLease`s
//! through a `styx-core` queue. A manual exposure ramp is requested frame-exactly.

use core::task::{Context, Poll, Waker};
use core::time::Duration;

use styx_core::buffer::{BackendFrameMeta, FrameLease, FrameMeta, NativeFrameMeta, PlaneLayout};
use styx_core::format::{ColorSpace, FourCc, MediaFormat, Resolution};
use styx_core::queue::{BoundedTx, QueueOverflow, RecvOutcome, bounded_with};
use styx_hal::Instant;
use styx_runtime::sync::{Lock, Ref, Shared, lock};
use styx_runtime::{
    Camera, CameraOptions, Controls, Frame, FrameControls, FrameStream, Platform, SensorState,
    instant_duration, serve_sync,
};
use styx_sensor::{ControlRequest, SensorDescription, SensorDriver};

use crate::Report;
use crate::board::{Dcmi, Pins, Registers, clock, expose, receiver_config, set_clock};

/// The sensor side on this board.
pub type Sensor = Lock<SensorState<Registers, Pins>>;

/// The platform: the DCMI-like receiver and the OV9782.
pub struct Mcu;

impl Platform for Mcu {
    type Receiver = Dcmi;
    type Sensor = Sensor;
}

/// Receiver buffers.
pub const BUFFERS: u32 = 3;

/// The frame period (30 fps).
const PERIOD_NS: u64 = 33_333_333;

/// The OV9782 description, compiled into the image at build time.
pub fn description() -> Option<SensorDescription> {
    SensorDescription::from_compiled(styx_sensor::include_description!("ov9782")).ok()
}

/// The board and camera: a receiver, the sensor brought up in its 1280x800 RAW10 mode (frames
/// cropped to `width` x `height` by the receiver), the camera over them, its controls.
pub struct Rig {
    /// The receiver.
    pub receiver: Ref<Dcmi>,
    /// The sensor side.
    pub sensor: Shared<SensorState<Registers, Pins>>,
    /// The camera.
    pub camera: Camera<Mcu>,
    /// Frame-exact controls.
    pub controls: Controls<Registers, Pins>,
    /// The stream, once started.
    pub frames: Option<FrameStream<Mcu>>,
    width: u32,
    height: u32,
    next: u64,
}

impl Rig {
    /// The camera for `width` x `height` frames, brought up; `None` if the sensor would not
    /// come up.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        set_clock(1_000_000_000);
        let desc = styx_sensor::Arc::new(description()?);
        let mut state =
            SensorState::new(SensorDriver::new(desc, Registers::default(), Pins)).with_clock(clock);
        state.bring_up("1280x800", "raw10").ok()?;
        let sensor = styx_runtime::sync::shared(state);
        let receiver = Ref::new(Dcmi::new(BUFFERS, width as usize * 2 * height as usize));
        Some(Self {
            controls: Controls::new(sensor.clone(), None),
            camera: Camera::new(receiver.clone(), sensor.clone(), CameraOptions::default()),
            receiver,
            sensor,
            frames: None,
            width,
            height,
            next: 0,
        })
    }

    /// Starts streaming.
    pub fn start(&mut self) -> bool {
        let config = receiver_config(self.width, self.height, BUFFERS);
        match self.camera.start(&config) {
            Ok((frames, _)) => {
                self.frames = Some(frames);
                true
            }
            Err(_) => false,
        }
    }

    /// One frame period on the board: the frame-start interrupt (the control schedule writes
    /// what is due), then the frame captured with what the sensor applied to it.
    pub fn tick(&mut self) {
        let seq = self.next;
        self.next += 1;
        let at = 1_000_000_000 + seq * PERIOD_NS;
        set_clock(at);
        self.receiver.frame_start(seq, Instant(at));
        if let Some(health) = self.camera.health() {
            let _ = serve_sync(&**self.camera.receiver(), &**self.camera.sensor(), health);
        }
        set_clock(at + PERIOD_NS * 9 / 10);
        let Some(applied) = lock(&self.sensor).applied(seq) else {
            return;
        };
        let exposure_us = applied.exposure.as_micros() as u32;
        let gain_q8 = (applied.gain() * 256.0) as u32;
        let width = self.width as usize;
        self.receiver.capture(seq, Instant(at), |out| {
            expose(out, width, exposure_us, gain_q8)
        });
    }

    /// The next finished frame, if one is done.
    pub fn poll(&mut self) -> Option<Frame<Dcmi>> {
        let frames = self.frames.as_mut()?;
        match frames.poll_frame(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Some(Ok(f))) => Some(f),
            _ => None,
        }
    }

    /// The values that produced frame `seq`.
    pub fn applied(&self, frame: &Frame<Dcmi>) -> Option<FrameControls> {
        frame
            .controls
            .or_else(|| self.controls.applied(frame.sequence))
    }

    /// Reports frame `seq`'s embedded data lines to the control schedule (read-back values
    /// replace the predicted ones). This board's receiver (a parallel DCMI) delivers none, so
    /// `data` is empty and nothing changes; the call keeps the decoding a CSI-2 board with
    /// embedded data uses in the image.
    pub fn report_embedded(&self, seq: u64, data: &[u8]) -> usize {
        lock(&self.sensor)
            .report_embedded(seq, data)
            .map_or(0, |m| m.len())
    }

    /// `frame` as a `FrameLease` over the receiver's buffer (no copy), with what produced it.
    pub fn lease(&self, frame: Frame<Dcmi>, c: &FrameControls) -> Option<FrameLease> {
        let (w, h) = (self.width, self.height);
        let stride = w as usize * 2;
        let format = MediaFormat::new(
            FourCc::new(*b"BG16"),
            Resolution::new(w, h)?,
            ColorSpace::Unknown,
        );
        let native = NativeFrameMeta {
            sequence: frame.sequence as u32,
            bytes_used: frame.bytes_used as u32,
            error: frame.corrupt,
            exposure_ns: c.exposure.as_nanos() as u64,
            analog_gain: c.analog_gain as f32,
            digital_gain: c.digital_gain as f32,
            frame_duration_ns: c.frame_duration.as_nanos() as u64,
            frame_length: c.frame_length,
            verified: c.verified,
        };
        let meta = FrameMeta::new(format, instant_duration(frame.timestamp).as_nanos() as u64)
            .with_backend(BackendFrameMeta::Native(native));
        let layout = PlaneLayout {
            offset: 0,
            len: stride * h as usize,
            stride,
        };
        Some(frame.into_lease(meta, smallvec::smallvec![layout]))
    }

    /// Stops streaming and powers the sensor down.
    pub fn shut_down(&mut self) {
        self.frames = None;
        let _ = self.camera.shut_down();
    }
}

/// A raw-frame queue that keeps the newest `depth` frames.
pub fn raw_queue(
    depth: usize,
) -> (
    BoundedTx<FrameLease>,
    styx_core::queue::BoundedRx<FrameLease>,
) {
    bounded_with(depth, QueueOverflow::DropOldest)
}

/// Exposures requested for frames still to come, by frame (a few frames ahead at most).
#[derive(Default)]
pub struct Expected([(u64, u64); 8]);

impl Expected {
    /// Frame `frame` is to be exposed `exposure_ns`.
    pub fn set(&mut self, frame: u64, exposure_ns: u64) {
        self.0[frame as usize % 8] = (frame, exposure_ns);
    }

    fn get(&self, frame: u64) -> Option<u64> {
        let (f, e) = self.0[frame as usize % 8];
        (f == frame && e > 0).then_some(e)
    }
}

/// The consumer: takes the newest raw frame, if one is queued, and reads its sequence, exposure
/// and first bytes; counts it as exact when its exposure is the one `expected` for it (to
/// within one line of 1280x800 at 30 fps).
pub fn consume(
    report: &mut Report,
    rx: &styx_core::queue::BoundedRx<FrameLease>,
    expected: &Expected,
) {
    let RecvOutcome::Data(lease) = rx.recv() else {
        return;
    };
    report.frames += 1;
    if let Some(BackendFrameMeta::Native(n)) = lease.meta().backend.as_ref() {
        report.mix(&n.exposure_ns.to_le_bytes());
        report.mix(&n.sequence.to_le_bytes());
        if expected
            .get(u64::from(n.sequence))
            .is_some_and(|e| e.abs_diff(n.exposure_ns) < 40_000)
        {
            report.exact += 1;
        }
    }
    let planes = lease.planes();
    report.mix(&planes[0].data()[..32]);
}

/// `frames` frames of `width` x `height`: raw frames to a consumer, the exposure ramped from
/// 5 ms by 0.5 ms a frame, each request landing frame-exactly on the frame the schedule
/// predicts (the consumer checks).
pub fn run(width: u32, height: u32, frames: u32) -> Report {
    let mut report = Report::new();
    let Some(mut rig) = Rig::new(width, height) else {
        return report;
    };
    let (tx, rx) = raw_queue(1);
    if !rig.start() {
        return report;
    }
    let mut expected = Expected::default();
    for _ in 0..frames {
        rig.tick();
        while let Some(frame) = rig.poll() {
            let seq = frame.sequence;
            let mismatches = rig.report_embedded(seq, core::hint::black_box(&[]));
            let exposure = Duration::from_micros(5_000 + 500 * ((seq + 1) % 20));
            let req = ControlRequest {
                exposure: Some(exposure),
                gain: Some(1.0),
                frame_duration: None,
            };
            // Asked for the next frame; the schedule says on which frame it lands (later when
            // the control delays do not leave time): the consumer checks that frame.
            let landings = rig.controls.request_at_now(seq + 1, &req);
            if let Ok(l) = &landings
                && let Some(first) = l.first()
                && l.iter().all(|l| l.frame == first.frame)
            {
                expected.set(first.frame, exposure.as_nanos() as u64);
            }
            report.mix(&[mismatches as u8, landings.map_or(0, |l| l.len()) as u8]);
            let Some(applied) = rig.applied(&frame) else {
                continue;
            };
            if let Some(lease) = rig.lease(frame, &applied) {
                let _ = tx.send(lease);
            }
            consume(&mut report, &rx, &expected);
        }
    }
    rig.shut_down();
    report
}

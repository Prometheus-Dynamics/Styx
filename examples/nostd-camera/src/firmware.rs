//! The firmware: a processed camera written once over any [`Platform`] whose sensor side is a
//! `styx-runtime` [`SensorState`]. A superloop drives it: [`Firmware::service`] at frame-start
//! interrupts (the frame-exact control schedule), [`Firmware::poll`] when frames are done
//! (the software ISP with 3A, the requests to the schedule, stills, metrics counters). Nothing
//! here knows the board.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use styx_algo::Tuning;
use styx_hal::{MaybeSendSync, Receiver, ReceiverConfig, SensorStart};
use styx_pipeline::still::{HeldRaw, StillPixels, soft_still_with};
use styx_pipeline::still_runner::{
    LoopReport, ShotExposure, StillOrder, StillOutcome, StillRunner, fixed_exposure_gain,
};
use styx_pipeline::{PipelineError, RawFrame, SensorInfo, SensorValues, SoftLoop};
use styx_runtime::metrics::{AaaSample, Counters, FrameSample};
use styx_runtime::sync::{Lock, MaybeSend, Shared};
use styx_runtime::{
    Camera, CameraOptions, Clock, Controls, DEFAULT_WRITE_MARGIN, FrameStream, Platform,
    SensorHandle, SensorState, instant_duration, serve_sync,
};
use styx_sensor::{RegisterBus, SensorPins};
use styx_softisp::{OutputBuffers, RawFormat, RawPacking, Scale};

/// What went wrong.
#[derive(Debug)]
pub enum FirmwareError {
    /// The camera (receiver, runtime).
    Camera(String),
    /// The processing loop.
    Pipeline(PipelineError),
}

impl From<PipelineError> for FirmwareError {
    fn from(e: PipelineError) -> Self {
        Self::Pipeline(e)
    }
}

/// Time from a frame's end to its statistics through the algorithms on this target (sets how
/// soon requests can be written).
const PROCESSING: Duration = Duration::from_millis(16);

/// FNV-1a over `bytes`: a cheap fingerprint of an image.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One processed frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameReport {
    /// What produced it (the control schedule's record).
    pub values: SensorValues,
    /// The digital gain it was processed with.
    pub digital_gain: f64,
    /// The algorithms ran on its statistics.
    pub ran: bool,
    /// AE locked after it.
    pub ae_locked: bool,
    /// AWB converged after it.
    pub awb_converged: bool,
    /// AE's total exposure (seconds × gain).
    pub total_exposure: f64,
    /// AWB's colour gains.
    pub colour_gains: [f64; 3],
    /// The frame the request made from it lands on.
    pub request_lands: Option<u64>,
    /// Fingerprint of the RGB image.
    pub image: u64,
    /// The image's mean R, G, B.
    pub means: [f64; 3],
}

impl FrameReport {
    /// One line with every value bit-exact (floats as their bits), for comparing two builds.
    pub fn trace_line(&self) -> String {
        let v = &self.values;
        format!(
            "{} {} {:016x} {:016x} {:016x} {} {} {:016x} {:016x} {:016x} {:016x} {:?} {:016x}",
            v.frame,
            v.exposure.as_nanos(),
            v.analogue_gain.to_bits(),
            v.digital_gain.to_bits(),
            self.digital_gain.to_bits(),
            self.ran,
            self.ae_locked,
            self.total_exposure.to_bits(),
            self.colour_gains[0].to_bits(),
            self.colour_gains[1].to_bits(),
            self.colour_gains[2].to_bits(),
            self.request_lands,
            self.image,
        )
    }
}

/// One still shot.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    /// The request.
    pub job: u32,
    /// Its frame.
    pub sequence: u64,
    /// Its bracket value.
    pub ev: f64,
    /// On the frame its exposure was to land on, with that exposure.
    pub landed: bool,
    /// What produced it.
    pub values: SensorValues,
    /// The reprocessed image (MHC demosaic, full size): fingerprint and mean green.
    pub image: u64,
    /// Mean green of the image.
    pub mean_green: f64,
}

/// The firmware's camera: see the [module documentation](self).
pub struct Firmware<P: Platform, B, Pn> {
    camera: Camera<P>,
    frames: Option<FrameStream<P>>,
    controls: Controls<B, Pn>,
    soft: SoftLoop,
    stills: StillRunner<u32, Box<HeldRaw>>,
    next_job: u32,
    /// Requests in flight and when they were made.
    requested: Vec<(u32, Duration)>,
    /// The camera's counters.
    pub counters: Counters,
    /// Shots taken.
    pub shots: Vec<Shot>,
    format: RawFormat,
    rgb: Vec<u8>,
    arithmetic: styx_softisp::Arithmetic,
    clock: Clock,
}

impl<P, B, Pn> Firmware<P, B, Pn>
where
    P: Platform<Sensor = Lock<SensorState<B, Pn>>>,
    B: RegisterBus + MaybeSend + 'static,
    Pn: SensorPins + MaybeSend + 'static,
    Lock<SensorState<B, Pn>>: MaybeSendSync,
    SensorHandle<P::Sensor>: SensorStart,
{
    /// A camera on `receiver` and `sensor` (brought up), processing frames of `info`'s mode
    /// stored as `packing` with `tuning`'s algorithms, on the platform `clock`.
    pub fn new(
        receiver: styx_runtime::sync::Ref<P::Receiver>,
        sensor: Shared<SensorState<B, Pn>>,
        info: SensorInfo,
        packing: RawPacking,
        tuning: &Tuning,
        clock: Clock,
    ) -> Result<Self, FirmwareError> {
        let format = RawFormat::new(info.width, info.height, info.cfa, packing);
        let rgb = vec![0u8; info.width as usize * info.height as usize * 3];
        let mut soft = SoftLoop::new(info, packing, tuning, 1)?;
        let latency = soft
            .info()
            .issue_latency(PROCESSING, DEFAULT_WRITE_MARGIN)
            .max(1);
        soft.controller().set_issue_latency(latency);
        Ok(Self {
            controls: Controls::new(sensor.clone(), None),
            camera: Camera::new(receiver, sensor, CameraOptions::default()),
            frames: None,
            soft,
            stills: StillRunner::new(),
            next_job: 0,
            requested: Vec::new(),
            counters: Counters::new(),
            shots: Vec::new(),
            format,
            rgb,
            arithmetic: styx_softisp::Arithmetic::Auto,
            clock,
        })
    }

    /// Resets the algorithms, writes their start-up exposure for frame 0 and starts streaming.
    pub fn start(&mut self, config: &ReceiverConfig) -> Result<(), FirmwareError> {
        self.soft.start_with(&self.controls)?;
        let (frames, _) = self
            .camera
            .start(config)
            .map_err(|e| FirmwareError::Camera(format!("{e}")))?;
        self.frames = Some(frames);
        Ok(())
    }

    /// Stops streaming and powers the sensor down.
    pub fn shut_down(&mut self) -> Result<(), FirmwareError> {
        self.frames = None;
        self.camera
            .shut_down()
            .map_err(|e| FirmwareError::Camera(format!("{e}")))
    }

    /// Serves frame starts: what the control schedule has due is written.
    pub fn service(&self) -> Result<(), FirmwareError> {
        let Some(health) = self.camera.health() else {
            return Ok(());
        };
        serve_sync(&**self.camera.receiver(), &**self.camera.sensor(), health)
            .map_err(|f| FirmwareError::Camera(format!("{f:?}")))
    }

    /// Asks for a still (taken over the next frames; see [`Self::shots`]). Returns its id.
    pub fn request_still(&mut self, exposure: ShotExposure, settle: bool) -> u32 {
        let job = self.next_job;
        self.next_job += 1;
        let now = (self.clock)();
        self.requested.push((job, now));
        let order = StillOrder {
            exposure,
            settle,
            timeout: Duration::from_secs(2),
            requested: now,
        };
        self.stills.submit(job, order);
        job
    }

    /// A finished frame, if one is done: processed, the algorithms run on it and their
    /// requests handed to the control schedule, stills and metrics recorded.
    pub fn poll(&mut self) -> Result<Option<FrameReport>, FirmwareError>
    where
        <P::Receiver as Receiver>::Error: core::fmt::Display,
    {
        let frames = self
            .frames
            .as_mut()
            .ok_or_else(|| FirmwareError::Camera("not started".into()))?;
        let frame = match frames.poll_frame(&mut Context::from_waker(Waker::noop())) {
            Poll::Pending => return Ok(None),
            Poll::Ready(None) => return Err(FirmwareError::Camera("stream ended".into())),
            Poll::Ready(Some(Err(e))) => return Err(FirmwareError::Camera(format!("{e}"))),
            Poll::Ready(Some(Ok(f))) => f,
        };
        let now = (self.clock)();
        if let Some(o) = self.stills.before_frame(&mut self.soft, now) {
            self.finish(o, now);
        }
        let seq = frame.sequence;
        let applied = frame
            .controls
            .or_else(|| self.controls.applied(seq))
            .ok_or_else(|| FirmwareError::Camera(format!("no values for frame {seq}")))?;
        let values = styx_pipeline::process::sensor_values(seq, &applied);
        let data = frame.data();
        let stride = self.format.width as usize * 2;
        let w = self.format.width as usize;
        let (out, lands) = self.soft.process_frame_with(
            RawFrame::Bytes(data),
            stride,
            &values,
            Scale::Full,
            OutputBuffers::Rgb24 {
                data: &mut self.rgb,
                stride: w * 3,
            },
            &self.controls,
        )?;
        let params = &out.step.params;
        let raw = self.stills.wants(&values).then(|| {
            let len = stride * self.format.height as usize;
            Box::new(HeldRaw {
                sequence: seq,
                timestamp: instant_duration(frame.timestamp),
                width: self.format.width,
                height: self.format.height,
                stride,
                packing: self.format.packing,
                cfa: self.format.pattern,
                bits: self.format.packing.bit_depth(),
                data: data[..len].to_vec(),
                sensor: values,
                isp: out.applied.clone(),
                params: Box::new(params.clone()),
            })
        });
        let report = LoopReport {
            lands,
            request: out.step.sensor,
            total_exposure: params.ae.total_exposure,
            ae_locked: params.ae.locked,
        };
        let frame_report = FrameReport {
            values,
            digital_gain: out.applied.digital_gain,
            ran: !out.stats.colour.is_empty(),
            ae_locked: params.ae.locked,
            awb_converged: params.awb.converged,
            total_exposure: params.ae.total_exposure,
            colour_gains: params.colour_gains,
            request_lands: lands,
            image: fingerprint(&self.rgb),
            means: means(&self.rgb),
        };
        // Metrics: the frame, its ISP time, the 3A state.
        let ts = instant_duration(frame.timestamp).as_nanos() as u64;
        let lost = self.counters.frame(&FrameSample {
            sequence: Some(seq),
            timestamp_ns: ts,
            now_ns: Some(now.as_nanos() as u64),
            corrupt: frame.corrupt,
            exposure: Some((
                values.exposure.as_nanos() as u64,
                values.analogue_gain as f32,
                values.digital_gain as f32,
            )),
        });
        self.counters.sequence_gaps.add(lost);
        let t = &out.timing;
        self.counters
            .isp_time(t.isp, t.settings + t.isp + t.stats + t.algorithms);
        self.counters.aaa.record(&AaaSample {
            ae_locked: params.ae.locked,
            awb_converged: params.awb.converged,
            colour_temperature: params.colour_temperature,
            lux: params.lux,
            flicker_period: params.ae.flicker_detected,
            af: None,
        });
        self.counters.received(ts, Some(now.as_nanos() as u64));
        drop(frame);
        if let Some(o) = self
            .stills
            .after_frame(&mut self.soft, &values, &report, raw)
        {
            self.finish(o, now);
        }
        Ok(Some(frame_report))
    }

    /// A still request ended: its shots reprocessed at full quality (inline: a superloop has
    /// no still thread; a firmware with an executor runs this in a low-priority task).
    fn finish(&mut self, outcome: StillOutcome<u32, Box<HeldRaw>>, now: Duration) {
        match outcome {
            StillOutcome::Failed { job, .. } => {
                self.requested(job);
                self.counters.stills.record(None);
            }
            StillOutcome::Taken { job, shots } => {
                let mut landed = 0;
                let n = shots.len() as u64;
                for mut shot in shots {
                    if let Some(want) = shot.want {
                        let raw = &mut shot.raw;
                        raw.isp.digital_gain =
                            fixed_exposure_gain(want, &raw.sensor, raw.params.colour_gains[1]);
                    }
                    let raw = &shot.raw;
                    let ok = shot.landed(raw.sequence, &raw.sensor);
                    landed += u64::from(ok);
                    let image =
                        soft_still_with(raw, &raw.isp, StillPixels::Rgb24, 1, self.arithmetic)
                            .unwrap_or_default();
                    self.shots.push(Shot {
                        job,
                        sequence: raw.sequence,
                        ev: shot.ev,
                        landed: ok,
                        values: raw.sensor,
                        image: fingerprint(&image),
                        mean_green: means(&image)[1],
                    });
                }
                let latency = self.requested(job).map_or(Duration::ZERO, |t| now - t);
                self.counters.stills.record(Some((latency, n, landed)));
            }
        }
    }

    /// When request `job` was made (forgotten here).
    fn requested(&mut self, job: u32) -> Option<Duration> {
        let i = self.requested.iter().position(|r| r.0 == job)?;
        Some(self.requested.swap_remove(i).1)
    }

    /// The software ISP's per-pixel arithmetic (default `Auto`: the fastest the CPU has, which
    /// differs between builds that detect CPU features at run time and those that cannot;
    /// `Int`, the reference, gives the same pictures everywhere).
    pub fn set_arithmetic(&mut self, arithmetic: styx_softisp::Arithmetic) {
        let base = styx_softisp::IspParams {
            arithmetic,
            ..styx_pipeline::soft::base_params()
        };
        self.soft.set_base_params(base);
        self.arithmetic = arithmetic;
    }

    /// The software loop (controls, tuning, controller).
    pub fn soft_loop(&mut self) -> &mut SoftLoop {
        &mut self.soft
    }

    /// The camera.
    pub fn camera(&self) -> &Camera<P> {
        &self.camera
    }
}

/// Mean R, G, B of an RGB24 image.
fn means(rgb: &[u8]) -> [f64; 3] {
    let mut s = [0u64; 3];
    for px in rgb.chunks_exact(3) {
        for c in 0..3 {
            s[c] += u64::from(px[c]);
        }
    }
    let n = (rgb.len() / 3).max(1) as f64;
    s.map(|v| v as f64 / n)
}

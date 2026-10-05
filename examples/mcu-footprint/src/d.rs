//! D: C + the software ISP. Each raw frame is processed into NV12 by `styx-softisp` with its
//! statistics in the same pass; the full 3A (black level, AWB, AE, ALSC, CCM, contrast) runs on
//! them through the pipeline core's software loop; a still bracket is taken once AE locks and
//! reprocessed at full quality; the metrics counters count frames, ISP time, the 3A state and
//! stills. Raw frames still go to a consumer as in B.

use alloc::boxed::Box;
use core::time::Duration;

use styx_algo::Tuning;
use styx_algo::tuning::AlscTuning;
use styx_pipeline::still::{HeldRaw, StillPixels, soft_still_with};
use styx_pipeline::still_runner::{
    LoopReport, ShotExposure, StillOrder, StillOutcome, StillRunner, fixed_exposure_gain,
};
use styx_pipeline::{RawFrame, SensorInfo, SoftLoop};
use styx_runtime::metrics::{AaaSample, Counters, FrameSample};
use styx_runtime::{DEFAULT_WRITE_MARGIN, instant_duration};
use styx_softisp::{Arithmetic, OutputBuffers, RawPacking, Scale};

use crate::Report;
use crate::b::{Expected, Rig, consume, description, raw_queue};
use crate::board::{clock, frame_buffer};

/// Time from a frame's end to its statistics through the algorithms (sets how soon requests
/// can be written).
const PROCESSING: Duration = Duration::from_millis(16);

/// Statistics zones across and down (also the lens shading grid: adaptive ALSC works on the
/// statistics' zones) and histogram bins: 8x6 and 64 where Linux uses 16x12 and 256, which is
/// plenty at QQVGA-VGA and a quarter of the memory (statistics, ALSC's matrices, lens shading
/// tables).
pub const STATS: (u32, u32, u32) = (8, 6, 64);

/// The tuning: the defaults with lens shading correction (adaptive ALSC) on the statistics'
/// grid.
pub fn tuning() -> Tuning {
    Tuning {
        alsc: Some(AlscTuning {
            grid: (STATS.0, STATS.1),
            ..AlscTuning::default()
        }),
        ..Tuning::default()
    }
}

/// The software ISP's fixed parameters: the loop's, with the reference arithmetic (`Int`:
/// what a Cortex-M runs anyway, the same pictures everywhere) and [`STATS`].
pub fn base_params() -> styx_softisp::IspParams {
    let base = styx_pipeline::soft::base_params();
    styx_softisp::IspParams {
        arithmetic: Arithmetic::Int,
        stats: base.stats.map(|s| styx_softisp::StatsConfig {
            zones_x: STATS.0,
            zones_y: STATS.1,
            histogram_bins: STATS.2,
            ..s
        }),
        ..base
    }
}

/// The OV9782's 1280x800 RAW10 mode as the algorithms see it, at `width` x `height`.
pub fn sensor_info(width: u32, height: u32) -> Option<SensorInfo> {
    let desc = description()?;
    let mut info = SensorInfo::from_description(&desc, "1280x800", "raw10")
        .ok()?
        .with_fps(30.0, 30.0)
        .ok()?;
    info.width = width;
    info.height = height;
    Some(info)
}

/// `frames` frames of `width` x `height` through the software ISP loop; a -1/0/+1 EV bracket
/// the frame after AE locks.
pub fn run(width: u32, height: u32, frames: u32) -> Report {
    run_with(width, height, frames, true)
}

/// [`run`], with the bracket only if `bracket` (it holds three raw frames until the last shot,
/// then reprocesses each: the heap's high-water mark).
pub fn run_with(width: u32, height: u32, frames: u32, bracket: bool) -> Report {
    let mut report = Report::new();
    let (Some(mut rig), Some(info)) = (Rig::new(width, height), sensor_info(width, height)) else {
        return report;
    };
    let packing = RawPacking::U16Le { bits: 10 };
    let Ok(mut soft) = SoftLoop::new(info, packing, &tuning(), 1) else {
        return report;
    };
    let latency = soft
        .info()
        .issue_latency(PROCESSING, DEFAULT_WRITE_MARGIN)
        .max(1);
    soft.controller().set_issue_latency(latency);
    soft.set_base_params(base_params());
    // Frames are in SRAM the CPU reads cached (a Cortex-M7 invalidates the D-cache over a
    // buffer when its lease begins): no staging copy of the input rows (16 KiB).
    soft.set_copy_input(false);
    let (w, h) = (width as usize, height as usize);
    let mut y = frame_buffer(w * h);
    let mut uv = frame_buffer(w * h / 2);
    let mut stills: StillRunner<u32, Box<HeldRaw>> = StillRunner::new();
    let mut counters = Counters::new();
    let mut requested: Option<Duration> = None;
    let (tx, rx) = raw_queue(1);
    if soft.start_with(&rig.controls).is_err() || !rig.start() {
        return report;
    }
    let expected = Expected::default();
    for _ in 0..frames {
        rig.tick();
        while let Some(frame) = rig.poll() {
            let now = clock();
            if let Some(o) = stills.before_frame(&mut soft, now) {
                finish(o, now, &mut requested, &mut counters, &mut report);
            }
            let seq = frame.sequence;
            rig.report_embedded(seq, core::hint::black_box(&[]));
            let Some(applied) = rig.applied(&frame) else {
                continue;
            };
            let values = styx_pipeline::process::sensor_values(seq, &applied);
            let timestamp = frame.timestamp;
            let Some(lease) = rig.lease(frame, &applied) else {
                continue;
            };
            let planes = lease.planes();
            let data = planes[0].data();
            let out = OutputBuffers::Nv12 {
                y: &mut y,
                y_stride: w,
                uv: &mut uv,
                uv_stride: w,
            };
            let Ok((out, lands)) = soft.process_frame_with(
                RawFrame::Bytes(data),
                w * 2,
                &values,
                Scale::Full,
                out,
                &rig.controls,
            ) else {
                continue;
            };
            let params = &out.step.params;
            let raw = stills.wants(&values).then(|| {
                Box::new(HeldRaw {
                    sequence: seq,
                    timestamp: instant_duration(timestamp),
                    width,
                    height,
                    stride: w * 2,
                    packing,
                    cfa: soft.format().pattern,
                    bits: 10,
                    data: data[..w * 2 * h].to_vec(),
                    sensor: values,
                    isp: out.applied.clone(),
                    params: Box::new(params.clone()),
                })
            });
            if bracket && params.ae.locked && requested.is_none() {
                requested = Some(now);
                let order = StillOrder {
                    exposure: ShotExposure::Bracket(alloc::vec![-1.0, 0.0, 1.0]),
                    settle: false,
                    timeout: Duration::from_secs(2),
                    requested: now,
                };
                stills.submit(0, order);
            }
            let loop_report = LoopReport {
                lands,
                request: out.step.sensor,
                total_exposure: params.ae.total_exposure,
                ae_locked: params.ae.locked,
            };
            // Metrics: the frame, its ISP time, the 3A state.
            let ts = instant_duration(timestamp).as_nanos() as u64;
            let lost = counters.frame(&FrameSample {
                sequence: Some(seq),
                timestamp_ns: ts,
                now_ns: Some(now.as_nanos() as u64),
                corrupt: false,
                exposure: Some((
                    values.exposure.as_nanos() as u64,
                    values.analogue_gain as f32,
                    values.digital_gain as f32,
                )),
            });
            counters.sequence_gaps.add(lost);
            let t = &out.timing;
            counters.isp_time(t.isp, t.settings + t.isp + t.stats + t.algorithms);
            counters.aaa.record(&AaaSample {
                ae_locked: params.ae.locked,
                awb_converged: params.awb.converged,
                colour_temperature: params.colour_temperature,
                lux: params.lux,
                flicker_period: params.ae.flicker_detected,
                af: None,
            });
            counters.received(ts, Some(now.as_nanos() as u64));
            report.ae(seq, params.ae.locked);
            report.mix(&y[..64]);
            report.mix(&uv[..32]);
            drop(planes);
            let _ = tx.send(lease);
            consume(&mut report, &rx, &expected);
            if let Some(o) = stills.after_frame(&mut soft, &values, &loop_report, raw) {
                finish(o, now, &mut requested, &mut counters, &mut report);
            }
        }
    }
    rig.shut_down();
    report.shots = counters.stills.shots.get();
    report.mix(&report.shots.to_le_bytes());
    report
}

/// A still request ended: its shots reprocessed at full quality (MHC demosaic, NV12).
fn finish(
    outcome: StillOutcome<u32, Box<HeldRaw>>,
    now: Duration,
    requested: &mut Option<Duration>,
    counters: &mut Counters,
    report: &mut Report,
) {
    match outcome {
        StillOutcome::Failed { .. } => counters.stills.record(None),
        StillOutcome::Taken { shots, .. } => {
            let n = shots.len() as u64;
            let mut landed = 0;
            for mut shot in shots {
                if let Some(want) = shot.want {
                    let raw = &mut shot.raw;
                    raw.isp.digital_gain =
                        fixed_exposure_gain(want, &raw.sensor, raw.params.colour_gains[1]);
                }
                let raw = &shot.raw;
                landed += u64::from(shot.landed(raw.sequence, &raw.sensor));
                if let Ok(image) =
                    soft_still_with(raw, &raw.isp, StillPixels::Nv12, 1, Arithmetic::Int)
                {
                    report.mix(&image[..64]);
                }
            }
            let latency = requested.map_or(Duration::ZERO, |t| now - t);
            counters.stills.record(Some((latency, n, landed)));
        }
    }
}

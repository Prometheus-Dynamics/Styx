//! C: B + AE and AWB, without an ISP: a mono or YUV sensor whose own ISP needs only exposure
//! control (and white balance gains written to it). Statistics are taken from the raw frames
//! (a coarse zone grid and a histogram over every fourth Bayer quad), the two algorithms run
//! on them, and their sensor requests go to the frame-exact control schedule.

use styx_algo::algos::{Agc, Awb};
use styx_algo::tuning::{AgcTuning, AwbTuning};
use styx_algo::{CameraConfig, FrameMetadata, Pipeline, Statistics, StatsAccumulator};
use styx_runtime::FrameControls;
use styx_sensor::ControlRequest;

use crate::Report;
use crate::b::{Expected, Rig, consume, raw_queue};

/// Statistics zones across and down, histogram bins.
const ZONES: (u32, u32, usize) = (8, 6, 64);

/// AE and AWB with the tuning's defaults.
pub fn algorithms() -> Option<Pipeline> {
    let mut p = Pipeline::new();
    p.push(Awb::new(AwbTuning::default()).ok()?);
    p.push(Agc::new(AgcTuning::default()).ok()?);
    Some(p)
}

/// The camera as the algorithms see it: the OV9782 mode's limits and control delays.
pub fn camera_config() -> Option<CameraConfig> {
    let desc = crate::b::description()?;
    let info = styx_pipeline::SensorInfo::from_description(&desc, "1280x800", "raw10").ok()?;
    Some(info.camera)
}

/// Statistics of a BGGR RAW10 frame (16-bit samples, black level 64): per-zone sums of every
/// fourth quad row's R, G, B, normalised to full scale, and a luma histogram.
pub fn raw_statistics(data: &[u8], width: u32, height: u32) -> Statistics {
    let (zx, zy, bins) = ZONES;
    let mut acc = StatsAccumulator::new(zx, zy, bins, 0.95);
    let sample = |x: usize, y: usize| {
        let i = (y * width as usize + x) * 2;
        let v = u16::from_le_bytes([data[i], data[i + 1]]);
        f64::from(v.saturating_sub(64)) / f64::from(1023 - 64)
    };
    let (qw, qh) = (width as usize / 2, height as usize / 2);
    for qy in (0..qh).step_by(4) {
        for qx in 0..qw {
            let (x, y) = (qx * 2, qy * 2);
            let b = sample(x, y);
            let g = (sample(x + 1, y) + sample(x, y + 1)) / 2.0;
            let r = sample(x + 1, y + 1);
            acc.add(
                (qx * zx as usize / qw) as u32,
                (qy * zy as usize / qh) as u32,
                r,
                g,
                b,
            );
        }
    }
    acc.finish()
}

/// What the algorithms see of a frame.
pub fn metadata(seq: u64, c: &FrameControls) -> FrameMetadata {
    FrameMetadata {
        frame: seq,
        exposure: c.exposure,
        analogue_gain: c.analog_gain,
        digital_gain: c.digital_gain,
        frame_duration: c.frame_duration,
        lux: None,
        lens: None,
        controls: Default::default(),
    }
}

/// `frames` frames of `width` x `height` with AE/AWB closing the loop over the control
/// schedule; raw frames to a consumer as in B.
pub fn run(width: u32, height: u32, frames: u32) -> Report {
    let mut report = Report::new();
    let Some(mut rig) = Rig::new(width, height) else {
        return report;
    };
    let (Some(mut algos), Some(config)) = (algorithms(), camera_config()) else {
        return report;
    };
    let Ok(start) = algos.prepare(&config) else {
        return report;
    };
    if let Some(r) = start.sensor {
        let req = ControlRequest {
            exposure: Some(r.exposure),
            gain: Some(r.analogue_gain),
            frame_duration: None,
        };
        let _ = rig.controls.request_at(0, &req);
    }
    let (tx, rx) = raw_queue(1);
    if !rig.start() {
        return report;
    }
    let expected = Expected::default();
    for _ in 0..frames {
        rig.tick();
        while let Some(frame) = rig.poll() {
            let seq = frame.sequence;
            rig.report_embedded(seq, core::hint::black_box(&[]));
            let Some(applied) = rig.applied(&frame) else {
                continue;
            };
            let stats = raw_statistics(frame.data(), width, height);
            let params = algos.process(&stats, &metadata(seq, &applied));
            if let Some(r) = params.sensor {
                let req = ControlRequest {
                    exposure: Some(r.exposure),
                    gain: Some(r.analogue_gain),
                    frame_duration: None,
                };
                let _ = rig.controls.request_at_now(r.frame.max(seq + 1), &req);
            }
            // White balance gains for the sensor's own ISP.
            for g in params.colour_gains {
                report.mix(&((g * 256.0) as u16).to_le_bytes());
            }
            report.ae(seq, params.ae.locked);
            if let Some(lease) = rig.lease(frame, &applied) {
                let _ = tx.send(lease);
            }
            consume(&mut report, &rx, &expected);
        }
    }
    rig.shut_down();
    report
}

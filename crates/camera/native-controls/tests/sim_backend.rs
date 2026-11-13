//! The delay proof on the simulated backend: `lemnosd` in this process on `lemnos-sim`, the
//! simulated sensor through its `camera.sensor/1` contract, and frames read as leases.
//!
//! The sensor's delays are non-zero and differ between runs (exposure 2 and gain 1, then exposure
//! 1 and gain 2). Native controls posts each step during the frame its delay says before its
//! target frame, and the simulated sensor latches each frame with the values that have landed on
//! it. Every frame is checked two ways: its brightness against the exposure and gain in force
//! on it, and its `FrameMetadata` against the same values. A step must show on its target frame,
//! not one earlier or later, 1,000 times for exposure, 1,000 for gain and 1,000 for the two posted
//! together.

mod common;

use std::time::Duration;

use common::{START, code_for_milli, code_for_us, us_for_code};
use styx_controls::{Control, ControlValue, FrameMetadata, Update};
use styx_native_controls::NativeControls;
use styx_sim::{SensorTiming, SimCamera, SimFrame};

/// Frames between two steps. More than the largest delay, so steps never share a post frame.
const SPACING: u64 = 4;
/// Steps per phase: exposure only, gain only, then both posted together.
const STEPS: u64 = 1_000;
const WIDTH: usize = 320;
const HEIGHT: usize = 40;
const ROW_BYTES: usize = WIDTH * 10 / 8;
/// Exposures and gains each step takes, cycling. Adjacent steps always differ, and the largest
/// product keeps every sample below full scale, so the brightness is never clipped.
const EXPOSURES_US: [u32; 4] = [5_000, 8_000, 10_000, 12_000];
const GAINS_MILLI: [u32; 3] = [1_000, 1_500, 1_250];

/// One step: the frame it shows on, and the values it sets (each optional).
#[derive(Debug, Clone, Copy)]
struct Step {
    target: u64,
    exposure_us: Option<u32>,
    gain_milli: Option<u32>,
}

/// The schedule: exposure-only steps, then gain-only steps, then both together.
fn schedule() -> Vec<Step> {
    let mut steps = Vec::new();
    let mut target = SPACING - 1;
    for i in 0..STEPS {
        steps.push(Step {
            target,
            exposure_us: Some(EXPOSURES_US[(i % 4) as usize]),
            gain_milli: None,
        });
        target += SPACING;
    }
    for i in 0..STEPS {
        steps.push(Step {
            target,
            exposure_us: None,
            gain_milli: Some(GAINS_MILLI[(i % 3) as usize]),
        });
        target += SPACING;
    }
    for i in 0..STEPS {
        steps.push(Step {
            target,
            exposure_us: Some(EXPOSURES_US[((i + 1) % 4) as usize]),
            gain_milli: Some(GAINS_MILLI[((i + 2) % 3) as usize]),
        });
        target += SPACING;
    }
    steps
}

/// Decodes a frame's `pRAA` plane into 10-bit samples.
fn samples(frame: &SimFrame) -> Vec<u16> {
    let read = frame.lease().read().expect("mapped for the CPU");
    let plane = read.plane(0).expect("plane 0");
    let mut out = Vec::with_capacity(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        let row = plane.row_bytes(y, ROW_BYTES).expect("row");
        for group in row.as_chunks::<5>().0 {
            for i in 0..4 {
                let high = u16::from(group[i]) << 2;
                let low = u16::from(group[4] >> (2 * i)) & 3;
                out.push(high | low);
            }
        }
    }
    out
}

fn mean(samples: &[u16]) -> f64 {
    samples.iter().map(|&s| f64::from(s)).sum::<f64>() / samples.len() as f64
}

/// Runs the schedule on a camera with `timing`, and checks every frame.
fn run(timing: SensorTiming) {
    let mut cam = SimCamera::start_with(styx_sim::Scene::Pattern, 0, timing)
        .expect("start the simulated camera with its delays");
    assert_eq!(
        cam.timing(),
        timing,
        "the sensor reports the delays it was given"
    );
    let mode = cam.sensor().mode(0).expect("mode 0");
    assert_eq!(mode.exposure_delay, timing.exposure_delay);
    assert_eq!(mode.gain_delay, timing.gain_delay);

    let mut controls = NativeControls::new(&mode, START).expect("native controls");
    controls.prepare(cam.sensor()).expect("initial values");

    let steps = schedule();
    let last = steps.last().expect("steps").target + 2;
    // Exposure and gain in force on each frame, from the schedule alone.
    let mut exposure_us = vec![START.exposure_us; (last + 1) as usize];
    let mut gain_milli = vec![START.gain_milli; (last + 1) as usize];
    let (mut exp, mut gain) = (START.exposure_us, START.gain_milli);
    let mut next_step = 0;
    for n in 0..=last {
        while next_step < steps.len() && steps[next_step].target == n {
            if let Some(e) = steps[next_step].exposure_us {
                exp = e;
            }
            if let Some(g) = steps[next_step].gain_milli {
                gain = g;
            }
            next_step += 1;
        }
        exposure_us[n as usize] = exp;
        gain_milli[n as usize] = gain;
    }

    let ref_exposure_code = code_for_us(u64::from(START.exposure_us));
    let ref_gain_code = code_for_milli(u64::from(START.gain_milli));
    let mut base_mean = None;
    let mut request_at: std::collections::BTreeMap<u64, Vec<Step>> = Default::default();
    for step in &steps {
        // Requested three frames before its target, so both delays post it on time (3 > 2).
        request_at.entry(step.target - 3).or_default().push(*step);
    }
    let mut landed = 0u64;

    for k in 0..=last {
        let frame = cam
            .next(Duration::from_secs(10))
            .expect("next")
            .expect("a frame");
        assert_eq!(frame.id(), k, "frames arrive in order");
        // Frame k is read: the values written now are written during frame k.
        controls.frame_start(cam.sensor(), k).expect("frame start");
        let mut updates = Vec::new();
        for step in request_at.get(&k).into_iter().flatten() {
            if let Some(e) = step.exposure_us {
                updates.push(Update {
                    control: Control::Exposure,
                    value: ControlValue::Manual(e),
                    frame: step.target,
                });
            }
            if let Some(g) = step.gain_milli {
                updates.push(Update {
                    control: Control::Gain,
                    value: ControlValue::Manual(g),
                    frame: step.target,
                });
            }
        }
        if !updates.is_empty() {
            let landings = controls
                .request(cam.sensor(), &updates)
                .expect("request accepted");
            for landing in landings.iter() {
                assert!(!landing.late(), "frame {k}: {landing:?} is late");
                assert_eq!(
                    landing.frame, landing.requested,
                    "{:?} lands on its target",
                    landing.control
                );
                landed += 1;
            }
        }
        // Check the frame that was read, against the schedule.
        let n = k as usize;
        let meta: &FrameMetadata = frame.meta();
        let e = exposure_us[n];
        let g = gain_milli[n];
        assert_eq!(
            meta.exposure_us,
            us_for_code(code_for_us(u64::from(e))) as u32,
            "frame {k}: exposure from the sensor"
        );
        assert_eq!(
            meta.analogue_gain_milli, g,
            "frame {k}: gain from the sensor"
        );
        let native = controls.metadata(k, frame.timestamp_ns());
        assert_eq!(
            native.exposure_us, meta.exposure_us,
            "frame {k}: native exposure"
        );
        assert_eq!(
            native.analogue_gain_milli, meta.analogue_gain_milli,
            "frame {k}: native gain"
        );

        // The scene's brightness is a constant times the scale of the exposure and gain in force:
        // frame 0 fixes the constant, and every later frame must match it.
        let image_mean = mean(&samples(&frame));
        let scale = code_for_us(u64::from(e)) as f64 / ref_exposure_code as f64
            * code_for_milli(u64::from(g)) as f64
            / ref_gain_code as f64;
        let base = *base_mean.get_or_insert(image_mean / scale);
        assert!(
            (image_mean - base * scale).abs() <= 1.0,
            "frame {k}: mean {image_mean:.2}, expected {:.2} at exposure {e} us, gain {g} milli",
            base * scale
        );

        cam.release(frame);
    }
    assert_eq!(cam.held_drops(), 0, "no frame was dropped");
    assert_eq!(
        landed,
        4 * STEPS,
        "every requested value landed: exposure, gain, and both together"
    );
    eprintln!(
        "proof: delays exposure={} gain={} steps={} frames={} landings={landed} held_drops=0",
        timing.exposure_delay,
        timing.gain_delay,
        steps.len(),
        last + 1
    );
}

#[test]
fn exposure_2_gain_1_steps_land_on_their_frame_1000_times() {
    run(SensorTiming {
        exposure_delay: 2,
        gain_delay: 1,
        frame_length_delay: 2,
    });
}

#[test]
fn exposure_1_gain_2_steps_land_on_their_frame_1000_times() {
    run(SensorTiming {
        exposure_delay: 1,
        gain_delay: 2,
        frame_length_delay: 2,
    });
}

//! The software ISP loop on a host: a frame's picture and statistics, the algorithms and their
//! requests, over many frames of a scene that keeps moving, counted with the allocator of
//! [`crate::pisp_alloc`] (`STYX_PISP_ALLOC_TRACE=1` prints what remains).

use std::time::Duration;

use styx_algo::Tuning;
use styx_pipeline::SoftLoop;
use styx_pipeline::controller::SensorValues;
use styx_softisp::{OutputBuffers, RawPacking, Scale};

use crate::pisp_alloc::{ARMED, counted, info, trace};

/// Frames of a moving scene (the algorithms on every frame), then of a still one the loop
/// settles on (the algorithms on some frames only).
const MOVING: u64 = 120;
const FRAMES: u64 = 360;
const WARM: u64 = 60;

#[test]
#[allow(clippy::print_stdout)]
fn the_soft_loop_allocates_per_frame() {
    trace::init();
    let info = info();
    let tuning = Tuning::load(concat!(env!("CARGO_MANIFEST_DIR"), "/tuning/ov9782.json")).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let mut soft = SoftLoop::new(info, RawPacking::Csi2Raw10, &tuning, 1).unwrap();
    soft.start().unwrap();
    let stride = soft.format().min_stride();
    let mut raw = vec![0u8; stride * h];
    let mut rgb = vec![0u8; 3 * (w / 2) * (h / 2)];
    let (mut exposure, mut gain, mut duration) = (
        Duration::from_millis(10),
        1.0,
        Duration::from_nanos(33_333_333),
    );
    let (mut total, mut frames, mut ran) = (0u64, 0u64, 0u64);
    let (mut settled, mut skipped) = (0u64, 0u64);
    for n in 0..FRAMES {
        // A grey scene with a gradient, its light moving, then still: what the sensor's
        // exposure and gain make of it.
        let light = if n < MOVING {
            1.0 + 0.3 * (n as f64 / 25.0).sin()
        } else {
            1.0
        };
        let level = light * 5000.0 * exposure.as_secs_f64() * gain;
        for (y, row) in raw.chunks_mut(stride).enumerate() {
            row.fill((level + (y / 16) as f64).min(250.0) as u8);
        }
        let values = SensorValues {
            frame: n,
            exposure,
            analogue_gain: gain,
            digital_gain: 1.0,
            frame_duration: duration,
            verified: true,
        };
        let steady = n >= WARM;
        ARMED.with(|a| a.set(steady));
        let ((), count) = counted(|| {
            let out = soft
                .process(
                    &raw,
                    stride,
                    &values,
                    Scale::Half,
                    OutputBuffers::Rgb24 {
                        data: &mut rgb,
                        stride: 3 * (w / 2),
                    },
                )
                .unwrap();
            if let Some(sensor) = out.step.sensor {
                exposure = sensor.exposure;
                gain = sensor.analogue_gain;
                duration = sensor.frame_duration;
            }
            let algorithms = !out.stats.colour.zones.is_empty();
            ran += u64::from(steady && algorithms);
            skipped += u64::from(steady && !algorithms);
            soft.recycle(out);
        });
        ARMED.with(|a| a.set(false));
        if steady && n >= MOVING {
            settled += count;
        }
        if steady {
            total += count;
            frames += 1;
            if std::env::var_os("STYX_PISP_PER_FRAME").is_some() && count > 0 {
                eprintln!("frame {n}: {count} allocations");
            }
        }
    }
    println!(
        "soft loop, steady state ({frames} frames, the algorithms on {ran}, skipped on {skipped}): \
         {:.2} allocations per frame ({settled} on the still scene)",
        total as f64 / frames as f64
    );
    trace::finish();
    assert!(
        skipped > 0,
        "the loop never settled: the skipped frames' path is not counted"
    );
    assert_eq!(total, 0, "the soft loop allocates");
}

//! The native path in CI: `lemnosd` in this process on the simulated backend, the simulated
//! sensor, receiver and ISP, frames read as leases and rendered in place. 1,000 frames, with
//! exposure and gain updates that must land on the frame each one names, and no copies.

use std::sync::Arc;
use std::time::Duration;

use styx_controls::{Control, ControlValue, Update, ValueError};
use styx_frame::copy_counters;
use styx_sim::{Error, Scene, SimCamera, SimFrame};

const FRAMES: u64 = 1_000;
const WIDTH: usize = 320;
const HEIGHT: usize = 40;
/// `pRAA`: five bytes per four samples.
const ROW_BYTES: usize = WIDTH * 10 / 8;

/// Decodes a frame's `pRAA` plane into 10-bit samples, row by row.
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

/// Exposure in force on frame `n`: the last update at or before it, from the schedule the test
/// set up (independent of the camera's own bookkeeping).
fn in_force(schedule: &[Update], control: Control, n: u64) -> Option<u32> {
    let mut value = None;
    for u in schedule
        .iter()
        .filter(|u| u.control == control && u.frame <= n)
    {
        value = match u.value {
            ControlValue::Manual(v) => Some(v),
            other => panic!("schedule uses {other:?}"),
        };
    }
    value
}

#[test]
fn thousand_frames_native_path_controls_land_on_their_frames_and_nothing_is_copied() {
    let mut cam = SimCamera::start(Scene::Pattern, 0).expect("start the simulated camera");
    assert_eq!((cam.width(), cam.height()), (WIDTH as u32, HEIGHT as u32));

    // Exposure and gain changes, each set before any frame is read, for frames further on.
    let exposures = [10_000u32, 5_000, 8_000];
    let gains = [1_000u32, 1_500];
    let mut schedule = Vec::new();
    for k in 1..=10u64 {
        schedule.push(Update {
            control: Control::Exposure,
            value: ControlValue::Manual(exposures[(k % 3) as usize]),
            frame: k * 97,
        });
        schedule.push(Update {
            control: Control::Gain,
            value: ControlValue::Manual(gains[(k % 2) as usize]),
            frame: k * 97 + 13,
        });
    }
    for update in &schedule {
        cam.set_control(*update)
            .expect("update accepted before its frame");
    }

    // Frames are real time: the simulated receiver makes one every 8.3 ms, and a consumer that
    // falls behind (a loaded host) loses frames to a full ring. Identities are checked, not
    // assumed contiguous: each frame must carry the controls its own identity names.
    let mut reference_mean = None;
    let mut last_id: Option<u64> = None;
    let mut gaps = 0u64;
    for _ in 0..FRAMES {
        let frame = loop {
            if let Some(frame) = cam.next(Duration::from_millis(500)).expect("next frame") {
                break frame;
            }
        };
        let n = frame.id();
        if let Some(last) = last_id {
            assert!(n > last, "frame {n} after frame {last}");
            gaps += n - last - 1;
        }
        last_id = Some(n);
        let exposure = in_force(&schedule, Control::Exposure, n).unwrap_or(10_000);
        let gain = in_force(&schedule, Control::Gain, n).unwrap_or(1_000);
        let meta = frame.meta();
        assert_eq!(
            meta.control(Control::Exposure),
            in_force(&schedule, Control::Exposure, n)
                .map_or(ControlValue::Auto, ControlValue::Manual),
            "frame {n}: exposure in force"
        );
        assert_eq!(meta.exposure_us, exposure, "frame {n}: exposure");
        assert_eq!(meta.analogue_gain_milli, gain, "frame {n}: gain");

        // The image is the scene at the frame's exposure and gain: the mean scales with both.
        let image_mean = mean(&samples(&frame));
        let base = *reference_mean.get_or_insert(image_mean);
        let expected = base * f64::from(exposure) / 10_000.0 * f64::from(gain) / 1_000.0;
        assert!(
            (image_mean - expected).abs() <= 1.0,
            "frame {n}: mean {image_mean:.2}, expected {expected:.2} at exposure {exposure}, gain {gain}"
        );
        cam.release(frame);
    }

    assert_eq!(cam.held_drops(), 0, "no frame named a buffer still held");
    let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
    assert_eq!(copies, 0, "frames were copied");
    eprintln!(
        "proof: frames={FRAMES} ids_in_order=true gaps_from_full_ring={gaps} held_drops={} \
         controls_scheduled={} exposure_changes={} gain_changes={} copies={copies}",
        cam.held_drops(),
        schedule.len(),
        schedule
            .iter()
            .filter(|u| u.control == Control::Exposure)
            .count(),
        schedule
            .iter()
            .filter(|u| u.control == Control::Gain)
            .count(),
    );
}

#[test]
fn a_late_update_and_a_bad_value_are_refused() {
    let mut cam = SimCamera::start(Scene::Pattern, 0).expect("start");
    let frame = cam
        .next(Duration::from_secs(5))
        .expect("next")
        .expect("a frame");
    assert_eq!(frame.id(), 0);
    cam.release(frame);

    let late = Update {
        control: Control::Exposure,
        value: ControlValue::Manual(5_000),
        frame: 0,
    };
    assert!(matches!(
        cam.set_control(late),
        Err(Error::Late {
            frame: 0,
            rendered: 0
        })
    ));

    let bad = Update {
        control: Control::Exposure,
        value: ControlValue::Manual(0),
        frame: 5,
    };
    assert!(matches!(
        cam.set_control(bad),
        Err(Error::Value(ValueError::OutOfRange { .. }))
    ));
}

#[test]
fn replayed_raw_frames_loop_and_scale_with_exposure() {
    let a = vec![512u16; WIDTH * HEIGHT];
    let b = vec![256u16; WIDTH * HEIGHT];
    let mut cam = SimCamera::start(Scene::Replay(Arc::new(vec![a, b])), 0).expect("start");
    // Recorded frame `identity % 2`: even identities are bright, odd ones half as bright.
    let mut bright = Vec::new();
    let mut dim = Vec::new();
    for _ in 0..3 {
        let frame = cam
            .next(Duration::from_secs(5))
            .expect("next")
            .expect("a frame");
        let m = mean(&samples(&frame));
        if frame.id().is_multiple_of(2) {
            bright.push(m);
        } else {
            dim.push(m);
        }
        cam.release(frame);
    }
    for m in &bright {
        assert!((m - bright[0]).abs() <= 1.0, "bright frames {bright:?}");
    }
    for m in &dim {
        assert!(
            (m * 2.0 - bright[0]).abs() <= 2.0,
            "dim {dim:?} bright {bright:?}"
        );
    }
}

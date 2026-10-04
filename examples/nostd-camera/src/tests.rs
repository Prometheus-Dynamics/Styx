//! The superloop on the host (only the test harness has `std`).

extern crate std;

use std::string::String;
use std::vec::Vec;

use super::*;

#[test]
fn the_camera_runs_converges_takes_a_bracket_and_counts() {
    let run = run(90);
    assert_eq!(run.frames.len(), 90);
    // Frames come in order with what the control schedule put on them.
    for (i, f) in run.frames.iter().enumerate() {
        assert_eq!(f.values.frame, i as u64);
    }
    // AE locks, on an exposure well above the recording's (the scene was recorded dark).
    let locked = run.ae_locked_at.expect("AE locks");
    assert!(locked < 60, "AE locked at frame {locked}");
    let last = run.frames.last().unwrap();
    assert!(last.ae_locked, "{last:?}");
    let total = last.values.exposure.as_secs_f64() * last.values.analogue_gain;
    assert!(total > 0.015, "{last:?}");
    // The exposure each frame got is what AE asked for when its request landed: the frame a
    // request names shows the request's exposure.
    for f in &run.frames {
        if let Some(at) = f.request_lands
            && let Some(later) = run.frames.get(at as usize)
        {
            assert!(later.values.frame == at);
        }
    }
    // Grey world takes out the warm cast: the picture's channels agree once settled.
    let [r, g, b] = last.means;
    assert!(
        (r / g - 1.0).abs() < 0.1 && (b / g - 1.0).abs() < 0.1,
        "{r} {g} {b}"
    );
    // The bracket: three shots on consecutive frames, each landed with its exposure, darker
    // to brighter.
    assert_eq!(run.shots.len(), 3, "{:?}", run.shots);
    let frames: Vec<u64> = run.shots.iter().map(|s| s.sequence).collect();
    assert_eq!(frames[1], frames[0] + 1);
    assert_eq!(frames[2], frames[1] + 1);
    assert!(run.shots.iter().all(|s| s.landed), "{:?}", run.shots);
    let totals: Vec<f64> = run
        .shots
        .iter()
        .map(|s| s.values.exposure.as_secs_f64() * s.values.analogue_gain)
        .collect();
    assert!(
        (totals[0] / totals[1] - 0.5).abs() < 0.05 && (totals[2] / totals[1] - 2.0).abs() < 0.1,
        "{totals:?}"
    );
    assert!(run.shots[0].mean_green < run.shots[2].mean_green);
    // The counters.
    let c = &run.counters;
    assert_eq!(c.frames.get(), 90);
    assert_eq!(c.received.get(), 90);
    assert_eq!(
        (
            c.sequence_gaps.get(),
            c.corrupted.get(),
            c.isp_skipped.get()
        ),
        (0, 0, 0)
    );
    let fps = c.measured_fps().unwrap();
    assert!((fps - 30.0).abs() < 0.01, "{fps}");
    // Frame start to processed: 90% of a frame period on the board clock.
    assert_eq!(c.delivery.quantile(0.5), Some(33_333_333 * 9 / 10));
    let aaa = c.aaa.read().unwrap();
    assert_eq!(aaa.ae_locked, Some(true));
    assert!(aaa.exposure.is_some() && aaa.colour_temperature.is_some());
    let s = &c.stills;
    assert_eq!(
        (
            s.requests.get(),
            s.shots.get(),
            s.landed.get(),
            s.failed.get()
        ),
        (1, 3, 3, 0)
    );
    assert!(run.register_writes > 0);

    // The trace, for comparing builds (scripts/check-nostd.sh): written where asked.
    if let Some(path) = std::env::var_os("STYX_NOSTD_TRACE") {
        let mut text = String::new();
        for f in &run.frames {
            text.push_str(&f.trace_line());
            text.push('\n');
        }
        for s in &run.shots {
            text.push_str(&std::format!(
                "shot {} {} {:016x} {:016x}\n",
                s.sequence,
                s.values.exposure.as_nanos(),
                s.values.analogue_gain.to_bits(),
                s.image
            ));
        }
        std::fs::write(path, text).unwrap();
    }
}

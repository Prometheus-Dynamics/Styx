//! The proof: a control value lands on the frame its delay says, on a sensor that keeps the mode's
//! delays (see `common`).

mod common;

use common::*;
use lemnos_contract_camera_sensor::{fps_milli_of, frame_length_for_fps_milli};
use styx_controls::{Control, ControlValue};

/// One exposure step, 1,000 times: the step frame and both delays vary, the request is made both
/// before its post frame and on it. The brightness and the metadata change on exactly that frame.
#[test]
fn an_exposure_step_lands_on_its_frame_1000_times() {
    let initial = code_for_us(8_000);
    let stepped = code_for_us(12_000);
    for i in 0..1_000u64 {
        let step = 10 + i % 97;
        let delay = (i % 4) as u8;
        let mode = mode(delay, (i / 4 % 4) as u8, 2);
        // Before the post frame (pending), or on it (posted at once).
        let request_at = if i % 2 == 0 {
            step - 4
        } else {
            step - u64::from(delay)
        };
        let end = step + 12;
        let run = run(
            &mode,
            request_at,
            &[update(Control::Exposure, 12_000, step)],
            end,
        )
        .unwrap_or_else(|e| panic!("iteration {i}: {e}"));

        assert_eq!(
            run.landings,
            [(Control::Exposure, step, step)],
            "iteration {i}"
        );
        for k in 0..=end {
            let want = if k < step { initial } else { stepped };
            assert_eq!(
                u64::from(run.sensor.exposure_at(k)),
                want,
                "iteration {i}: exposure in force on frame {k} (step on {step}, delay {delay})"
            );
            let meta = run.controls.metadata(k, 0);
            assert_eq!(
                meta.exposure_us,
                report_us(u64::from(run.sensor.exposure_at(k))),
                "iteration {i}: metadata on frame {k}"
            );
            assert_eq!(
                meta.control(Control::Exposure),
                ControlValue::Manual(meta.exposure_us)
            );
            assert_eq!(
                meta.exposure_us,
                report_us(u64::from(run.sensor.exposure_at(k))),
                "iteration {i}: metadata against the sensor on frame {k}"
            );
            assert_eq!(
                run.sensor.brightness(k),
                u64::from(run.sensor.exposure_at(k)) * code_for_milli(1_000),
                "iteration {i}: brightness on frame {k}"
            );
        }
        // One write, made in the frame the delay says before the step, for the step frame.
        assert_eq!(
            run.sensor.posts,
            vec![(step - u64::from(delay), step)],
            "iteration {i}: post frame and identity"
        );
    }
}

/// Exposure and gain stepped together, 1,000 times, with their delays varied independently: both
/// land on the step frame every time. With different delays the two writes are in different frames
/// (staggered); with the same delay they share one write.
#[test]
fn exposure_and_gain_stepped_together_land_on_the_same_frame_1000_times() {
    for i in 0..1_000u64 {
        let step = 10 + i % 89;
        let ed = (i % 4) as u8;
        let gd = (i / 4 % 4) as u8;
        let mode = mode(ed, gd, 2);
        // On time for both controls: the request comes no later than the longer delay's post frame.
        let request_at = if i % 3 == 0 {
            step - 4
        } else {
            step - u64::from(ed.max(gd))
        };
        let end = step + 12;
        let run = run(
            &mode,
            request_at,
            &[
                update(Control::Exposure, 12_000, step),
                update(Control::Gain, 2_000, step),
            ],
            end,
        )
        .unwrap_or_else(|e| panic!("iteration {i}: {e}"));

        assert_eq!(
            run.landings,
            [(Control::Exposure, step, step), (Control::Gain, step, step)],
            "iteration {i}"
        );
        for k in 0..=end {
            let (exp, gain) = if k < step {
                (code_for_us(8_000), code_for_milli(1_000))
            } else {
                (code_for_us(12_000), code_for_milli(2_000))
            };
            assert_eq!(
                u64::from(run.sensor.exposure_at(k)),
                exp,
                "iteration {i}: exposure on {k}"
            );
            assert_eq!(
                u64::from(run.sensor.gain_at(k)),
                gain,
                "iteration {i}: gain on {k}"
            );
            let meta = run.controls.metadata(k, 0);
            assert_eq!(
                meta.analogue_gain_milli,
                if k < step { 1_000 } else { 2_000 },
                "iteration {i}: metadata gain on {k}"
            );
            assert_eq!(
                meta.exposure_us,
                report_us(u64::from(run.sensor.exposure_at(k))),
                "iteration {i}: metadata exposure on {k}"
            );
            assert_eq!(
                run.sensor.brightness(k),
                exp * gain,
                "iteration {i}: brightness on {k}"
            );
        }
        let want_posts: Vec<(u64, u64)> = if ed == gd {
            vec![(step - u64::from(ed), step)]
        } else {
            let mut v = vec![(step - u64::from(ed), step), (step - u64::from(gd), step)];
            v.sort_unstable();
            v
        };
        let mut got = run.sensor.posts.clone();
        got.sort_unstable();
        assert_eq!(got, want_posts, "iteration {i}: post frames");
    }
}

/// A request made after its frame has passed is posted at once and lands later; the landing says so.
#[test]
fn a_late_request_lands_late_and_says_so() {
    for delay in 0..4u8 {
        let mode = mode(delay, delay, 2);
        let run = run(&mode, 21, &[update(Control::Exposure, 12_000, 20)], 30).unwrap();
        assert_eq!(
            run.landings,
            [(Control::Exposure, 20, 21 + u64::from(delay))],
            "delay {delay}"
        );
        assert_eq!(
            run.sensor.exposure_at(20 + u64::from(delay)),
            code_for_us(8_000) as u32
        );
        assert_eq!(
            run.sensor.exposure_at(21 + u64::from(delay)),
            code_for_us(12_000) as u32
        );
        assert_eq!(
            run.controls
                .metadata(21 + u64::from(delay) - 1, 0)
                .exposure_us,
            report_us(code_for_us(8_000)),
        );
        assert_eq!(
            run.controls.metadata(21 + u64::from(delay), 0).exposure_us,
            report_us(code_for_us(12_000)),
        );
    }
}

/// A frame rate lands on its frame length delay, and the metadata reports the rate the whole frame
/// length gives (the contract's rate for the whole-line frame, not the rate asked for).
#[test]
fn a_frame_rate_lands_on_the_frame_length_delay() {
    let mode = mode(2, 2, 2);
    let (lines_60, rate_60) =
        frame_length_for_fps_milli(LINE_TIME_PS, 1, u32::MAX, 60_000).unwrap();
    let (lines_30, _) = frame_length_for_fps_milli(LINE_TIME_PS, 1, u32::MAX, 30_000).unwrap();
    assert_eq!(rate_60, fps_milli_of(LINE_TIME_PS, lines_60).unwrap());
    // Written in frame 8, shown from frame 10.
    let run = run(&mode, 8, &[update(Control::FrameRate, 30_000, 10)], 16).unwrap();
    assert_eq!(run.landings, [(Control::FrameRate, 10, 10)]);
    assert_eq!(run.sensor.frame_length, vec![(0, lines_60), (10, lines_30)]);
    assert_eq!(
        run.controls.metadata(9, 0).control(Control::FrameRate),
        ControlValue::Manual(rate_60)
    );
    assert_eq!(
        run.controls.metadata(10, 0).control(Control::FrameRate),
        ControlValue::Manual(30_000)
    );
}

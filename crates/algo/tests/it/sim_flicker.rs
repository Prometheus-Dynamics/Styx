//! Mains flicker with the device's timing: AE must not chase the beat of 100 Hz light on
//! exposures shorter than a period (120 fps), must quantise longer ones, must detect the mains
//! frequency on its own, and must never oscillate over long runs with flicker and noise.
//!
//! Run with `--nocapture` to see the measurements.

use crate::common;

use std::time::Duration;

use styx_algo::sim::{Scene, SceneFlicker, SensorModel, SimFrame, Simulation};
use styx_algo::{CameraConfig, ControlDelays, Flicker, Pipeline};

/// The OV9782 as the PiSP path drives it: delays 2/2/1, requests written in the frame of their
/// statistics at 30 and 60 fps, one frame later at 120 fps.
fn device(fps: f64) -> CameraConfig {
    let fd = Duration::from_secs_f64(1.0 / fps);
    CameraConfig {
        exposure_limits: (Duration::from_micros(20), fd - Duration::from_micros(200)),
        exposure_margin: Duration::from_micros(200),
        frame_duration_limits: (fd, fd),
        analogue_gain_limits: (1.0, 15.9375),
        delays: ControlDelays {
            exposure: 2,
            analogue_gain: 2,
            frame_duration: 1,
            issue_latency: if fps > 90.0 { 1 } else { 0 },
        },
        unsettled_frames: 1,
        ..Default::default()
    }
}

/// `frames` frames at `fps` under `lux` of light flickering at `hz` with `depth`.
fn run(fps: f64, lux: f64, hz: f64, depth: f64, flicker: Flicker, frames: u64) -> Vec<SimFrame> {
    run_at(0.0, fps, lux, hz, depth, flicker, frames)
}

/// [`run`] with the clock (the flicker's phase) starting at `t0` seconds.
fn run_at(
    t0: f64,
    fps: f64,
    lux: f64,
    hz: f64,
    depth: f64,
    flicker: Flicker,
    frames: u64,
) -> Vec<SimFrame> {
    let mut scene = Scene::constant(lux, 3500.0);
    scene.flicker = (depth > 0.0).then_some(SceneFlicker { hz, depth });
    run_scene(t0, fps, scene, flicker, frames)
}

fn run_scene(t0: f64, fps: f64, scene: Scene, flicker: Flicker, frames: u64) -> Vec<SimFrame> {
    let config = device(fps);
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    let init = p.prepare(&config).unwrap().clone();
    let sensor = SensorModel {
        seed: 7,
        ..Default::default()
    };
    let mut sim = Simulation::new(sensor, scene, &config);
    let s = init.sensor.unwrap();
    sim.start_with(
        s.exposure.as_secs_f64(),
        s.analogue_gain,
        s.frame_duration.as_secs_f64(),
    );
    sim.controls.flicker = flicker;
    sim.set_time(t0);
    let out = sim.run(&mut p, frames);
    assert_eq!(sim.late_landings(), 0);
    out
}

/// Measurements over `frames[from..]` (printed; not every field is asserted on).
#[derive(Debug)]
#[allow(dead_code)]
struct Steady {
    /// Relative standard deviation of exposure × gain (AE moving).
    total_sd: f64,
    /// Largest relative step of exposure × gain between frames.
    max_step: f64,
    /// Exposure × gain changes (frames whose value differs from the previous one by > 0.5%).
    changes: usize,
    /// Relative standard deviation of the raw mean luma, frame to frame.
    luma_sd: f64,
    /// Fraction of frames AE reports locked.
    locked: f64,
    /// First frame AE reported locked.
    first_lock: Option<usize>,
    /// The last frame's exposure (ms).
    exposure_ms: f64,
}

fn steady(frames: &[SimFrame], from: usize) -> Steady {
    let total = |f: &SimFrame| f.meta.exposure.as_secs_f64() * f.meta.analogue_gain;
    let rel_sd = |v: &[f64]| {
        let m = v.iter().sum::<f64>() / v.len() as f64;
        (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt() / m
    };
    let tail = &frames[from..];
    let totals: Vec<f64> = tail.iter().map(total).collect();
    let steps: Vec<f64> = totals
        .windows(2)
        .map(|w| (w[1] / w[0] - 1.0).abs())
        .collect();
    let luma: Vec<f64> = tail.iter().map(|f| f.raw_y).collect();
    Steady {
        total_sd: rel_sd(&totals),
        max_step: steps.iter().fold(0.0f64, |a, b| a.max(*b)),
        changes: steps.iter().filter(|s| **s > 0.005).count(),
        luma_sd: rel_sd(&luma),
        locked: tail.iter().filter(|f| f.params.ae.locked).count() as f64 / tail.len() as f64,
        first_lock: frames.iter().position(|f| f.params.ae.locked),
        exposure_ms: tail[tail.len() - 1].meta.exposure.as_secs_f64() * 1e3,
    }
}

#[test]
fn ae_does_not_chase_100_hz_at_120_fps() {
    // 8.1 ms exposures at the most (frame 8.33 ms): 81% of a 100 Hz period; depth 0.3 puts
    // ±7% on the frames, 0.5 about ±12%.
    for depth in [0.3, 0.5] {
        let off = steady(&run(120.0, 150.0, 100.0, depth, Flicker::Off, 600), 120);
        let on = steady(&run(120.0, 150.0, 100.0, depth, Flicker::Mains50, 600), 120);
        let auto = steady(&run(120.0, 150.0, 100.0, depth, Flicker::Auto, 600), 120);
        println!("120 fps, 100 Hz depth {depth}: off {off:?}");
        println!("120 fps, 100 Hz depth {depth}: 50 Hz {on:?}");
        println!("120 fps, 100 Hz depth {depth}: auto {auto:?}");
        for s in [&on, &auto] {
            assert!(s.total_sd < 0.01, "{s:?}");
            assert!(s.locked > 0.97, "{s:?}");
            assert!(s.changes < 20, "{s:?}");
        }
        assert!(on.total_sd < off.total_sd / 2.0, "{on:?} vs {off:?}");
    }
}

#[test]
fn flicker_is_detected_at_the_alias_frequencies() {
    // 50 Hz mains at 30 / 60 / 120 fps (10 / 20 / 20 Hz beat) with exposures shorter than a
    // period; 60 Hz mains at 90 fps (30 Hz beat). The mains frequency off by 0.07 Hz.
    for (fps, lux, hz, want) in [
        (30.0, 2000.0, 100.07, 0.010),
        (60.0, 2000.0, 100.07, 0.010),
        (120.0, 150.0, 100.07, 0.010),
        (90.0, 2000.0, 120.07, 1.0 / 120.0),
    ] {
        let frames = run(fps, lux, hz, 0.4, Flicker::Auto, (fps * 2.0) as u64);
        let at = frames
            .iter()
            .position(|f| f.params.ae.flicker_detected.is_some());
        let last = frames.last().unwrap();
        println!(
            "{fps} fps, {hz} Hz: detected at frame {at:?} ({:?}), exposure {:?}",
            last.params.ae.flicker_detected, last.meta.exposure
        );
        let got = last.params.ae.flicker_detected.map(|d| d.as_secs_f64());
        assert!(
            got.is_some_and(|g| (g - want).abs() < 1e-6),
            "{fps}: {got:?}"
        );
        assert!(at.is_some_and(|a| (a as f64) < fps * 1.5), "{fps}: {at:?}");
        // Exposures of a period or more are whole periods from then on.
        let p = want;
        for f in &frames[frames.len() - 10..] {
            let t = f.meta.exposure.as_secs_f64();
            if t >= p {
                let n = t / p;
                assert!((n - n.round()).abs() < 0.01, "{fps}: {t}");
            }
        }
    }
    // Steady light, and 60 Hz mains at 120 fps (no beat): nothing detected.
    for (hz, depth) in [(100.0, 0.0), (120.0, 0.5)] {
        let frames = run(120.0, 150.0, hz, depth, Flicker::Auto, 360);
        assert!(
            frames
                .iter()
                .all(|f| f.params.ae.flicker_detected.is_none()),
            "{hz} {depth}"
        );
    }
}

#[test]
fn quantised_exposures_once_detected_at_30_fps() {
    // Dim enough for exposures past one period: whole 10 ms periods, gain makes up the rest.
    let frames = run(30.0, 40.0, 100.0, 0.5, Flicker::Auto, 300);
    let s = steady(&frames, 150);
    let t = frames.last().unwrap().meta.exposure.as_secs_f64();
    println!("30 fps, 40 lux, auto: exposure {t}, {s:?}");
    assert!(
        t >= 0.0099 && ((t / 0.01) - (t / 0.01).round()).abs() < 0.01,
        "{t}"
    );
    assert!(s.luma_sd < 0.01, "{s:?}");
    assert!(s.total_sd < 0.005, "{s:?}");
}

#[test]
fn ae_never_oscillates_over_long_runs() {
    // 20 s at 120 fps, 30 s at 30 and 60 fps, flicker (off-nominal mains) and sensor noise,
    // avoidance off, set and automatic: after start-up exposure × gain never steps by more
    // than the flicker could explain, and AE stays locked.
    for (fps, lux, frames) in [(120.0, 150.0, 2400), (60.0, 450.0, 1800), (30.0, 40.0, 900)] {
        for flicker in [Flicker::Off, Flicker::Mains50, Flicker::Auto] {
            let out = run(fps, lux, 100.03, 0.15, flicker, frames);
            let s = steady(&out, (fps * 2.0) as usize);
            println!("{fps} fps, {lux} lux, {flicker:?}: {s:?}");
            assert!(s.total_sd < 0.03, "{fps} {flicker:?}: {s:?}");
            assert!(s.max_step < 0.1, "{fps} {flicker:?}: {s:?}");
            if flicker != Flicker::Off {
                assert!(s.total_sd < 0.01, "{fps} {flicker:?}: {s:?}");
                assert!(s.locked > 0.98, "{fps} {flicker:?}: {s:?}");
            }
        }
    }
}

#[test]
fn cold_start_at_120_fps_locks_whatever_the_flicker_phase() {
    // ±3.5% on the frames (depth 0.15, as measured on the device), 12 phases of the light
    // against the stream start.
    for flicker in [Flicker::Off, Flicker::Mains50, Flicker::Auto] {
        let locks: Vec<usize> = (0..12)
            .map(|k| {
                let frames = run_at(
                    f64::from(k) / 1200.0,
                    120.0,
                    150.0,
                    100.0,
                    0.15,
                    flicker,
                    120,
                );
                steady(&frames, 60).first_lock.unwrap_or(usize::MAX)
            })
            .collect();
        println!("120 fps cold start, 100 Hz depth 0.15, {flicker:?}: locked at {locks:?}");
        if flicker != Flicker::Off {
            assert!(locks.iter().all(|l| *l <= 12), "{flicker:?}: {locks:?}");
        }
    }
}

#[test]
fn a_lamp_flickering_at_the_mains_frequency() {
    // The device's room: a lamp on 50 Hz mains with a 50 Hz component (half-wave driver) and
    // smaller ones at 100 and 150 Hz. Exposures avoid it in whole 20 ms periods where they
    // can (30 fps), AE meters against the mean light where they cannot (60 and 120 fps).
    for (fps, lux) in [(120.0, 150.0), (60.0, 450.0), (30.0, 40.0)] {
        let mut scene = Scene::constant(lux, 3500.0);
        scene.flicker = Some(SceneFlicker {
            hz: 50.02,
            depth: 0.25,
        });
        scene.flicker_harmonics = vec![
            SceneFlicker {
                hz: 100.04,
                depth: 0.1,
            },
            SceneFlicker {
                hz: 150.06,
                depth: 0.03,
            },
        ];
        let frames = (fps * 10.0) as u64;
        let off = steady(
            &run_scene(0.0, fps, scene.clone(), Flicker::Off, frames),
            (fps * 3.0) as usize,
        );
        let out = run_scene(0.0, fps, scene, Flicker::Auto, frames);
        let auto = steady(&out, (fps * 3.0) as usize);
        let last = &out.last().unwrap().params.ae;
        println!("{fps} fps, 50 Hz lamp: off {off:?}");
        println!(
            "{fps} fps, 50 Hz lamp: auto {auto:?}, {:?} / {:?}",
            last.flicker_detected, last.flicker_period
        );
        assert_eq!(
            last.flicker_detected,
            Some(Duration::from_millis(10)),
            "{fps}"
        );
        assert_eq!(
            last.flicker_period,
            Some(Duration::from_millis(20)),
            "{fps}"
        );
        assert!(
            auto.total_sd < 0.01 && auto.locked > 0.98,
            "{fps}: {auto:?}"
        );
        if fps <= 30.0 {
            let t = auto.exposure_ms / 20.0;
            assert!((t - t.round()).abs() < 0.01, "{auto:?}");
            assert!(auto.luma_sd < 0.01, "{auto:?}");
        }
    }
}

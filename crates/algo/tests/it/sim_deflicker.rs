//! Deflicker with the device's timing: the flicker the model predicts for each frame divided
//! out with the ISP's digital gain, computed (as on the PiSP path) from the parameters of the
//! frame before for the frame being processed.
//!
//! Run with `--nocapture` to see the measurements.

use crate::common;

use std::time::Duration;

use styx_algo::sim::{Scene, SceneFlicker, SensorModel, SimFrame, Simulation};
use styx_algo::{CameraConfig, ControlDelays, Deflicker, Flicker, Pipeline};

const MAX_DIGITAL_GAIN: f64 = 4.0;

/// The OV9782 as the PiSP path drives it (as `sim_flicker.rs`).
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

/// The room's lamp on mains `mains` Hz: ±25% at the mains frequency, ±10% and ±3% at its
/// second and third harmonics.
fn lamp(lux: f64, mains: f64) -> Scene {
    let mut scene = Scene::constant(lux, 3500.0);
    scene.flicker = Some(SceneFlicker {
        hz: mains,
        depth: 0.25,
    });
    scene.flicker_harmonics = vec![
        SceneFlicker {
            hz: 2.0 * mains,
            depth: 0.1,
        },
        SceneFlicker {
            hz: 3.0 * mains,
            depth: 0.03,
        },
    ];
    scene
}

fn run(
    fps: f64,
    scene: Scene,
    flicker: Flicker,
    deflicker: Deflicker,
    frames: u64,
) -> Vec<SimFrame> {
    let config = device(fps);
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    let init = p.prepare(&config).unwrap().clone();
    let sensor = SensorModel {
        seed: 11,
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
    sim.controls.deflicker = deflicker;
    let out = sim.run(&mut p, frames);
    assert_eq!(sim.late_landings(), 0);
    out
}

/// The output's brightness per frame: the raw mean times the ISP gain the frame got from the
/// parameters of the frame before (as the PiSP path: frame F goes through the back end with
/// the settings from F − 1, its digital gain recomputed for what F got).
fn output(frames: &[SimFrame]) -> Vec<(f64, f64)> {
    frames
        .windows(2)
        .map(|w| {
            let g = w[0].params.frame_gain(&w[1].meta, MAX_DIGITAL_GAIN);
            (w[1].raw_y * g.digital_gain, g.digital_gain)
        })
        .collect()
}

#[derive(Debug)]
#[allow(dead_code)]
struct Measured {
    /// Relative standard deviation of the output's brightness.
    out_sd: f64,
    /// Mean relative change of the output's brightness from frame to frame.
    out_step: f64,
    /// The raw frames' relative standard deviation (the flicker as the sensor saw it).
    raw_sd: f64,
    /// Exposure × gain changes (> 0.5%) of the sensor.
    changes: usize,
    /// Frames AE reports locked.
    locked: f64,
    /// Smallest and largest ISP gain.
    gains: (f64, f64),
    /// Correction strength at the end and the headroom.
    strength: f64,
    headroom: f64,
    exposure_ms: f64,
}

fn rel_sd(v: &[f64]) -> f64 {
    let m = v.iter().sum::<f64>() / v.len() as f64;
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt() / m
}

fn measure(frames: &[SimFrame], from: usize) -> Measured {
    let out = output(frames);
    let tail = &out[from - 1..];
    let y: Vec<f64> = tail.iter().map(|o| o.0).collect();
    let mean = y.iter().sum::<f64>() / y.len() as f64;
    let raw: Vec<f64> = frames[from..].iter().map(|f| f.raw_y).collect();
    let totals: Vec<f64> = frames[from..]
        .iter()
        .map(|f| f.meta.exposure.as_secs_f64() * f.meta.analogue_gain)
        .collect();
    let last = frames.last().unwrap();
    let d = last.params.deflicker.as_ref();
    Measured {
        out_sd: rel_sd(&y),
        out_step: y.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>()
            / (y.len() - 1) as f64
            / mean,
        raw_sd: rel_sd(&raw),
        changes: totals
            .windows(2)
            .filter(|w| (w[1] / w[0] - 1.0).abs() > 0.005)
            .count(),
        locked: frames[from..].iter().filter(|f| f.params.ae.locked).count() as f64
            / (frames.len() - from) as f64,
        gains: tail
            .iter()
            .fold((f64::MAX, 0.0f64), |(lo, hi), o| (lo.min(o.1), hi.max(o.1))),
        strength: d.map_or(0.0, |d| d.strength),
        headroom: d.map_or(1.0, |d| d.headroom),
        exposure_ms: last.meta.exposure.as_secs_f64() * 1e3,
    }
}

#[test]
fn the_room_lamp_is_taken_out_at_60_90_and_120_fps() {
    // Mains 0.07 Hz off nominal (as measured in the frames' clock on the CM5).
    for (fps, lux) in [(120.0, 150.0), (90.0, 250.0), (60.0, 450.0)] {
        let frames = (fps * 10.0) as u64;
        let from = (fps * 3.0) as usize;
        let off = measure(
            &run(fps, lamp(lux, 50.07), Flicker::Auto, Deflicker::Off, frames),
            from,
        );
        let on = measure(
            &run(
                fps,
                lamp(lux, 50.07),
                Flicker::Auto,
                Deflicker::Auto,
                frames,
            ),
            from,
        );
        println!("{fps} fps, 50.07 Hz lamp, deflicker off: {off:?}");
        println!("{fps} fps, 50.07 Hz lamp, deflicker auto: {on:?}");
        assert!(off.out_sd > 0.1, "{fps}: {off:?}");
        assert!(on.out_sd < 0.003, "{fps}: {on:?}");
        // No highlight near clipping: no headroom, the sensor keeps its exposure (no AE
        // moves), gains on both sides of 1; AE locked.
        assert!(on.headroom < 1.05, "{fps}: {on:?}");
        assert!(
            (on.exposure_ms / off.exposure_ms - 1.0).abs() < 0.02,
            "{fps}: {on:?}"
        );
        assert!(on.gains.0 > 0.6 && on.gains.1 < 1.6, "{fps}: {on:?}");
        assert!(on.changes == 0 && on.locked > 0.98, "{fps}: {on:?}");
        assert_eq!(on.strength, 1.0);
    }
}

#[test]
fn highlights_get_headroom() {
    // A lamp in view (a patch far above the rest, clipped in every frame): the sensor leaves
    // headroom for the brightest frame and no frame gets a gain below 1, so the clipped patch
    // stays white; the flicker is taken out all the same.
    let mut scene = lamp(150.0, 50.07);
    scene.reflectance[20] = [4.0; 3];
    let off = measure(
        &run(120.0, scene.clone(), Flicker::Auto, Deflicker::Off, 1200),
        360,
    );
    let on = measure(
        &run(120.0, scene, Flicker::Auto, Deflicker::Auto, 1200),
        360,
    );
    println!("120 fps, lamp in view: off {off:?}");
    println!("120 fps, lamp in view: auto {on:?}");
    assert!(on.headroom > 1.2 && on.gains.0 >= 1.0, "{on:?}");
    assert!(on.out_sd < 0.01 && on.out_sd < off.out_sd / 8.0, "{on:?}");
    assert!(on.changes == 0 && on.locked > 0.98, "{on:?}");
}

/// [`run`] with a noisier sensor (fewer, noisier samples per zone).
fn run_noisy(fps: f64, scene: Scene, deflicker: Deflicker, frames: u64) -> Vec<SimFrame> {
    let config = device(fps);
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    p.prepare(&config).unwrap();
    let sensor = SensorModel {
        seed: 5,
        shot_noise: 2e-3,
        read_noise: 4e-3,
        samples_per_zone: 4,
        ..Default::default()
    };
    let mut sim = Simulation::new(sensor, scene, &config);
    sim.controls.flicker = Flicker::Auto;
    sim.controls.deflicker = deflicker;
    sim.run(&mut p, frames)
}

#[test]
fn sixty_hz_mains_and_noise() {
    // 60 Hz mains (0.1 Hz off) at 90 fps, a noisier sensor: taken out down to the frames'
    // own noise (the same sensor under steady light), which the correction does not add to.
    let mut scene = lamp(150.0, 60.1);
    scene.flicker_harmonics.truncate(1);
    let m = measure(&run_noisy(90.0, scene, Deflicker::Auto, 900), 270);
    let steady = measure(
        &run_noisy(90.0, Scene::constant(150.0, 3500.0), Deflicker::Auto, 900),
        270,
    );
    println!("90 fps, 60.1 Hz lamp, noisy: {m:?}");
    println!("90 fps, steady light, noisy: {steady:?}");
    assert!(m.out_sd < m.raw_sd / 10.0, "{m:?}");
    assert!(
        m.out_sd < steady.out_sd * 1.5 + 0.002,
        "{m:?} vs {steady:?}"
    );
}

#[test]
fn steady_light_is_left_alone() {
    // No flicker: no correction, the same output as without deflicker, and nothing that
    // keeps the algorithms from their settled rate.
    for fps in [120.0, 30.0] {
        let scene = Scene::constant(150.0, 3500.0);
        let on = run(fps, scene.clone(), Flicker::Auto, Deflicker::On, 600);
        let off = run(fps, scene, Flicker::Auto, Deflicker::Off, 600);
        assert!(on.iter().all(|f| f.params.deflicker.is_none()), "{fps}");
        let (a, b) = (output(&on), output(&off));
        assert_eq!(a, b, "{fps}");
    }
}

#[test]
fn deflicker_on_works_without_flicker_avoidance() {
    let frames = run(120.0, lamp(150.0, 49.95), Flicker::Off, Deflicker::On, 1200);
    let m = measure(&frames, 360);
    println!("120 fps, avoidance off, deflicker on: {m:?}");
    assert!(m.out_sd < 0.006, "{m:?}");
    // Auto follows avoidance: off here.
    let frames = run(
        120.0,
        lamp(150.0, 49.95),
        Flicker::Off,
        Deflicker::Auto,
        600,
    );
    assert!(frames.iter().all(|f| f.params.deflicker.is_none()));
}

#[test]
fn hand_off_to_whole_periods_and_back() {
    // 30 fps: the light dims from 2000 to 40 lux and back, exposures go from short ones
    // (corrected) to whole 20 ms periods (nothing to correct) and back. The output never
    // jumps more than the light's own step would and ends flat both ways.
    let fps = 30.0;
    let mut scene = lamp(2000.0, 50.03);
    scene.lux = styx_algo::Pwl::new(vec![
        (0.0, 2000.0),
        (300.0, 2000.0),
        (330.0, 40.0),
        (600.0, 40.0),
        (630.0, 2000.0),
        (900.0, 2000.0),
    ])
    .unwrap();
    let on = run(fps, scene.clone(), Flicker::Auto, Deflicker::Auto, 900);
    let off = run(fps, scene, Flicker::Auto, Deflicker::Off, 900);
    let seg = |frames: &[SimFrame], a: usize, b: usize| {
        let y: Vec<f64> = output(frames)[a..b].iter().map(|o| o.0).collect();
        (rel_sd(&y), frames[b].meta.exposure.as_secs_f64())
    };
    // Largest frame-to-frame change of the output over the transitions.
    let jump = |frames: &[SimFrame]| {
        let out = output(frames);
        [(295, 400), (595, 700)]
            .iter()
            .flat_map(|&(a, b)| out[a..b].windows(2).map(|w| (w[1].0 / w[0].0 - 1.0).abs()))
            .fold(0.0f64, f64::max)
    };
    let (short, long, back) = (seg(&on, 200, 299), seg(&on, 500, 599), seg(&on, 800, 899));
    let (short_off, back_off) = (seg(&off, 200, 299), seg(&off, 800, 899));
    println!(
        "30 fps hand-off: short {short:?} (off {short_off:?}), whole periods {long:?}, back \
         {back:?} (off {back_off:?}); largest step {:.4} (off {:.4})",
        jump(&on),
        jump(&off)
    );
    assert!(short.1 < 0.02 && back.1 < 0.02, "{short:?} {back:?}");
    let n = long.1 / 0.02;
    assert!((n - n.round()).abs() < 0.01 && n > 0.99, "{long:?}");
    assert!(long.0 < 0.003, "{long:?}");
    // At 30 fps 50 and 100 Hz alias onto frequencies 0.09 Hz apart (mains 0.03 Hz off), which
    // a second of frames cannot tell apart: a few percent stay of the short exposures' 10%.
    for (s, o) in [(short, short_off), (back, back_off)] {
        assert!(s.0 < 0.03 && s.0 < o.0 / 4.0, "{s:?} vs {o:?}");
    }
    assert!(
        jump(&on) < jump(&off) * 1.1 + 0.01,
        "{} vs {}",
        jump(&on),
        jump(&off)
    );
}

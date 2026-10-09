//! Autofocus in simulation (`sim::FocusSim`): a subject at a distance, the IMX708 module's
//! lens model (VCM settle time, hysteresis, noise), focus statistics from the lens's real
//! position, optional phase data. Run with `--nocapture` for the numbers in algorithms.md.

use crate::common;

use std::time::Duration;

use styx_algo::sim::{FocusScene, FocusSim, LensModel, Optics, PdafModel, Scene, SimFrame};
use styx_algo::{AfMode, AfState, CameraConfig, ControlDelays, Pipeline, Pwl, Tuning};

/// The PiSP path at 30 fps: requests written in the frame the statistics came from.
fn config() -> CameraConfig {
    let mut c = common::config();
    c.frame_duration_limits = (
        Duration::from_nanos(33_333_333),
        Duration::from_nanos(33_333_333),
    );
    c.exposure_limits = (Duration::from_micros(20), Duration::from_millis(33));
    c.delays = ControlDelays {
        exposure: 2,
        analogue_gain: 2,
        frame_duration: 1,
        issue_latency: 0,
    };
    c
}

/// The simulation tuning plus Raspberry Pi's IMX708 AF section (imx708.json, BSD-2-Clause).
fn tuning(pdaf: bool) -> Tuning {
    let mut t = common::tuning();
    let mut af = Tuning::from_rpi_json_str(IMX708_AF)
        .unwrap()
        .tuning
        .af
        .unwrap();
    if !pdaf {
        // No phase data: CDAF only (as libcamera's tunings for sensors without PDAF).
        af.speeds.normal.dropout_frames = 0;
    }
    t.af = Some(af);
    t
}

const IMX708_AF: &str = r#"{"version": 2.0, "algorithms": [{"rpi.af": {
    "ranges": {"normal": {"min": 0.0, "max": 12.0, "default": 1.0},
               "macro": {"min": 3.0, "max": 15.0, "default": 4.0}},
    "speeds": {"normal": {"step_coarse": 1.0, "step_fine": 0.25, "contrast_ratio": 0.75,
        "retrigger_ratio": 0.8, "retrigger_delay": 10, "pdaf_gain": -0.016,
        "pdaf_squelch": 0.125, "max_slew": 1.5, "pdaf_frames": 20, "dropout_frames": 6,
        "step_frames": 5},
        "fast": {"step_coarse": 1.25, "step_fine": 0.0, "contrast_ratio": 0.75,
        "retrigger_ratio": 0.8, "retrigger_delay": 8, "pdaf_gain": -0.02,
        "pdaf_squelch": 0.125, "max_slew": 2.0, "pdaf_frames": 16, "dropout_frames": 4,
        "step_frames": 4}},
    "conf_epsilon": 8, "conf_thresh": 16, "conf_clip": 512, "skip_frames": 5,
    "check_for_ir": false, "map": [0.0, 445, 15.0, 925]}}]}"#;

struct Run {
    p: Pipeline,
    sim: styx_algo::sim::Simulation,
    frames: Vec<SimFrame>,
}

impl Run {
    fn new(lux: f64, scene: FocusScene, optics: Optics, pdaf: bool, mode: AfMode) -> Self {
        let mut config = config();
        let focus = FocusSim::new(scene, LensModel::default(), optics);
        config.lens = Some(focus.lens_config());
        let (p, mut sim) = common::start(&tuning(pdaf), &config, Scene::constant(lux, 4500.0));
        let mut focus = focus;
        let start = p.params().lens.expect("a lens request at start");
        focus.start_at(start.position);
        sim.focus = Some(focus);
        sim.controls.af_mode = mode;
        Self {
            p,
            sim,
            frames: Vec::new(),
        }
    }

    fn run(&mut self, n: u64) {
        let f = self.sim.run(&mut self.p, n);
        self.frames.extend(f);
    }

    fn trigger(&mut self) {
        self.sim.controls.af_trigger += 1;
    }

    fn state(&self, i: usize) -> AfState {
        self.frames[i].params.af.state
    }

    /// The lens's true position (dioptres) during frame `i`.
    fn lens(&self, i: usize) -> f64 {
        self.sim.focus.as_ref().unwrap().history[i]
    }

    /// First frame from `from` on with `state`, and after it the state never changes.
    fn settled_in(&self, from: usize, state: AfState) -> Option<usize> {
        let n = self.frames.len();
        (from..n).find(|&i| (i..n).all(|j| self.state(j) == state))
    }

    /// Frames in `range` where the commanded lens position changed.
    fn moves(&self, range: std::ops::Range<usize>) -> usize {
        range
            .filter(|&i| i > 0)
            .filter(|&i| {
                self.frames[i].params.af.lens_position != self.frames[i - 1].params.af.lens_position
            })
            .count()
    }

    fn print(&self, label: &str, range: std::ops::Range<usize>) {
        for i in range {
            let f = &self.frames[i];
            println!(
                "{label} {i:3} {:?} cmd {:.2} true {:.2} rep {:?} contrast {:.3e} noise {:.3} phase {:.0} conf {:.0}",
                f.params.af.state,
                f.params.af.lens_position.unwrap_or(-1.0),
                self.lens(i),
                f.meta.lens.map(|l| (l.position, l.settled)),
                f.params.af.contrast,
                f.params.af.contrast_noise,
                f.params.af.phase,
                f.params.af.confidence,
            );
        }
    }
}

#[test]
fn one_shot_finds_the_peak() {
    for (subject, report) in [(2.5, true), (0.4, true), (7.0, true), (2.5, false)] {
        let mut r = Run::new(
            300.0,
            FocusScene::subject_at(subject),
            Optics::default(),
            false,
            AfMode::Auto,
        );
        r.run(10);
        r.sim.focus.as_mut().unwrap().report = report;
        r.trigger();
        r.run(140);
        if std::env::var_os("STYX_AF_TRACE").is_some() {
            r.print("one-shot", 0..r.frames.len());
        }
        let done = r
            .settled_in(10, AfState::Focused)
            .unwrap_or_else(|| panic!("subject {subject}: never focused"));
        let end = r.frames.len() - 1;
        let err = r.lens(end) - subject;
        println!(
            "one-shot, subject {subject} D, lens reports {report}: focused at frame {} \
             ({} frames after the trigger), lens {:.3} D (error {err:+.3} D)",
            done,
            done - 10,
            r.lens(end)
        );
        assert!(
            err.abs() < 0.15,
            "subject {subject}: lens {:.3}",
            r.lens(end)
        );
        assert!(
            done - 10 <= if report { 60 } else { 90 },
            "took {}",
            done - 10
        );
        assert_eq!(r.moves(done..end + 1), 0, "the lens stays once focused");
    }
}

#[test]
fn continuous_refocuses_after_a_depth_change_without_hunting() {
    let mut scene = FocusScene::subject_at(1.0);
    scene.subject = Scene::step(200, 1.0, 4.0);
    let mut r = Run::new(300.0, scene, Optics::default(), false, AfMode::Continuous);
    r.run(450);
    if std::env::var_os("STYX_AF_TRACE").is_some() {
        r.print("caf", 0..r.frames.len());
    }
    let at = |i: usize| r.lens(i);
    assert!((at(199) - 1.0).abs() < 0.15, "before: {:.3}", at(199));
    let first_done = (0..200)
        .find(|&i| (i..200).all(|j| r.state(j) == AfState::Focused))
        .expect("focused before the change");
    assert_eq!(
        r.moves(first_done + 1..200),
        0,
        "no hunting while the scene is still"
    );
    let refocused = r.settled_in(200, AfState::Focused).unwrap();
    let end = r.frames.len() - 1;
    println!(
        "continuous: focused at frame {first_done} (1.00 D, lens {:.3}); subject to 4 D at 200: \
         refocused at frame {refocused} ({} frames), lens {:.3} D; lens moves after: {}",
        at(199),
        refocused - 200,
        at(end),
        r.moves(refocused..end + 1)
    );
    assert!((at(end) - 4.0).abs() < 0.15, "after: {:.3}", at(end));
    assert!(refocused - 200 <= 90, "refocus took {}", refocused - 200);
    assert_eq!(r.moves(refocused..end + 1), 0, "no hunting once refocused");
}

#[test]
fn low_light_and_no_texture_fail_gracefully() {
    // Too dark: the contrast curve is noise.
    for (label, lux, texture) in [("0.05 lux", 0.05, 0.05), ("no texture", 300.0, 0.0)] {
        let mut scene = FocusScene::subject_at(2.5);
        scene.texture = Pwl::constant(texture);
        let mut r = Run::new(lux, scene.clone(), Optics::default(), false, AfMode::Auto);
        r.run(10);
        r.trigger();
        r.run(140);
        let end = r.frames.len() - 1;
        let state = r.state(end);
        println!(
            "{label}: one-shot ended {state:?} at frame {:?}, lens {:.2} D",
            r.settled_in(10, state),
            r.lens(end)
        );
        assert_eq!(state, AfState::Failed, "{label}");
        // Back at the default (hyperfocal) position.
        assert!(
            (r.lens(end) - 1.0).abs() < 0.15,
            "{label}: lens {:.2}",
            r.lens(end)
        );
        // Continuous: one scan, then it waits for the scene to change (no endless rescans).
        let mut c = Run::new(lux, scene, Optics::default(), false, AfMode::Continuous);
        c.run(400);
        let scans = (0..c.frames.len())
            .filter(|&i| {
                c.state(i) == AfState::Scanning && (i == 0 || c.state(i - 1) != AfState::Scanning)
            })
            .count();
        println!(
            "{label}: continuous: {scans} scan(s) in 400 frames, ends {:?}",
            c.state(399)
        );
        assert!(scans <= 2, "{label}: {scans} scans");
        assert_eq!(c.state(399), AfState::Failed);
    }
}

#[test]
fn pdaf_converges_faster_than_contrast_scans() {
    let optics = Optics {
        pdaf: Some(PdafModel::default()),
        ..Optics::default()
    };
    let mut frames = Vec::new();
    for pdaf in [false, true] {
        let mut r = Run::new(
            300.0,
            FocusScene::subject_at(3.0),
            if pdaf { optics } else { Optics::default() },
            pdaf,
            AfMode::Auto,
        );
        r.run(10);
        r.trigger();
        r.run(140);
        if std::env::var_os("STYX_AF_TRACE").is_some() {
            r.print(if pdaf { "pdaf" } else { "cdaf" }, 0..r.frames.len());
        }
        let end = r.frames.len() - 1;
        // Within 0.1 D of the subject for good.
        let near = (10..=end)
            .find(|&i| (i..=end).all(|j| (r.lens(j) - 3.0).abs() < 0.1))
            .unwrap_or(end);
        println!(
            "one-shot to 3 D from 1 D, {}: lens within 0.1 D from frame {} ({} frames), \
             state {:?}, lens {:.3} D",
            if pdaf { "PDAF" } else { "CDAF" },
            near,
            near - 10,
            r.state(end),
            r.lens(end)
        );
        assert_eq!(r.state(end), AfState::Focused);
        frames.push(near - 10);
    }
    assert!(
        frames[1] * 2 <= frames[0],
        "PDAF {} vs CDAF {}",
        frames[1],
        frames[0]
    );
    // Continuous with PDAF: follows a moving subject.
    let mut scene = FocusScene::subject_at(1.0);
    scene.subject = Pwl::new(vec![(100.0, 1.0), (160.0, 5.0)]).unwrap();
    let mut r = Run::new(300.0, scene, optics, true, AfMode::Continuous);
    r.run(260);
    let lag: Vec<f64> = (120..260)
        .map(|i| {
            (r.lens(i)
                - r.sim
                    .focus
                    .as_ref()
                    .unwrap()
                    .scene
                    .subject
                    .eval_clamped(i as f64))
            .abs()
        })
        .collect();
    let worst_after = lag[60..].iter().cloned().fold(0.0, f64::max);
    println!(
        "continuous PDAF, subject 1 → 5 D over frames 100-160: largest error during the move \
         {:.2} D, after {:.3} D",
        lag[..40].iter().cloned().fold(0.0, f64::max),
        worst_after
    );
    assert!(worst_after < 0.15);
}

#[test]
fn manual_lens_position_and_no_lens() {
    let mut r = Run::new(
        300.0,
        FocusScene::subject_at(2.0),
        Optics::default(),
        false,
        AfMode::Manual,
    );
    r.sim.controls.lens_position = Some(5.0);
    r.run(40);
    assert_eq!(r.state(39), AfState::Idle);
    assert!((r.lens(39) - 5.0).abs() < 0.1, "{}", r.lens(39));
    // Without a lens AF is inert.
    let (mut p, mut sim) = common::start(&tuning(false), &config(), Scene::constant(300.0, 4500.0));
    sim.controls.af_mode = AfMode::Continuous;
    let f = sim.run(&mut p, 5);
    assert!(
        f.iter()
            .all(|f| f.params.lens.is_none() && !f.params.af.active)
    );
}

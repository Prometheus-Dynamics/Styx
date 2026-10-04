use alloc::vec;
use alloc::vec::Vec;

use super::*;

fn cam() -> CameraConfig {
    CameraConfig {
        exposure_limits: (Duration::from_micros(20), Duration::from_millis(100)),
        exposure_margin: Duration::from_micros(500),
        frame_duration_limits: (Duration::from_millis(33), Duration::from_millis(33)),
        analogue_gain_limits: (1.0, 16.0),
        ..CameraConfig::default()
    }
}

fn values(frame: u64, exposure: Duration, gain: f64) -> SensorValues {
    SensorValues {
        frame,
        exposure,
        analogue_gain: gain,
        digital_gain: 1.0,
        frame_duration: Duration::from_millis(33),
        verified: true,
    }
}

#[test]
fn exposures_split_within_the_frame() {
    let c = cam();
    // 10 ms at gain 2: more keeps the gain until the frame is full.
    let (t, g) = split_exposure(0.020, 2.0, &c);
    assert_eq!((t, g), (Duration::from_millis(10), 2.0));
    let (t, g) = split_exposure(0.080, 2.0, &c);
    assert_eq!(t, Duration::from_micros(32_500));
    assert!((g - 0.080 / 0.0325).abs() < 1e-9);
    // Less: shorter exposure at the same gain; below gain 1 the exposure gives.
    let (t, g) = split_exposure(0.002, 4.0, &c);
    assert_eq!((t, g), (Duration::from_micros(500), 4.0));
    let (t, g) = split_exposure(0.0001, 0.5, &c);
    assert_eq!((t, g), (Duration::from_micros(100), 1.0));
}

#[test]
fn targets_wait_for_their_exposure_then_give_up() {
    let t = Target {
        frame: 10,
        want: Some((Duration::from_millis(10), 2.0)),
    };
    let s = |frame, ms: u64, g| values(frame, Duration::from_millis(ms), g);
    assert!(!t.wants(&s(9, 10, 2.0)));
    assert!(t.wants(&s(10, 10, 2.0)));
    assert!(!t.wants(&s(11, 20, 2.0)));
    assert!(t.wants(&s(14, 20, 2.0)));
    // The gain a fixed shot is processed with: the green gain, and what the sensor fell
    // short of.
    let want = (Duration::from_millis(10), 2.0);
    assert_eq!(fixed_exposure_gain(want, &s(10, 10, 2.0), 1.5), 1.5);
    assert_eq!(fixed_exposure_gain(want, &s(10, 5, 2.0), 1.0), 2.0);
}

/// The 3A loop as the still runner sees it: controls handed over, AE asking for them.
#[derive(Default)]
struct FakeLoop {
    controls: Vec<Controls>,
}

impl StillHost for FakeLoop {
    fn set_controls(&mut self, c: Controls) {
        self.controls.push(c);
    }

    fn controls(&mut self) -> Controls {
        Controls::default()
    }

    fn camera(&mut self) -> CameraConfig {
        cam()
    }
}

fn order(exposure: ShotExposure) -> StillOrder {
    StillOrder {
        exposure,
        settle: false,
        timeout: Duration::from_secs(1),
        requested: Duration::ZERO,
    }
}

/// Runs `frames` frames: requests made at frame f land at f + 3, frames show what landed.
/// Returns the outcome and the frame it came on.
fn run(
    runner: &mut StillRunner<u32, u64>,
    host: &mut FakeLoop,
    frames: u64,
) -> Option<(u64, StillOutcome<u32, u64>)> {
    let mut sensor = values(0, Duration::from_millis(10), 2.0);
    let mut landing: Vec<(u64, (Duration, f64))> = Vec::new();
    for f in 0..frames {
        let now = Duration::from_millis(33 * f);
        if let Some(o) = runner.before_frame(host, now) {
            return Some((f, o));
        }
        sensor.frame = f;
        if let Some(&(_, (e, g))) = landing.iter().rev().find(|(at, _)| *at <= f) {
            (sensor.exposure, sensor.analogue_gain) = (e, g);
        }
        let raw = runner.wants(&sensor).then_some(f);
        // AE asks for the last controls handed over (or AE's own 10 ms x 2).
        let c = host.controls.last().cloned().unwrap_or_default();
        let want = match (c.exposure, c.analogue_gain) {
            (Some(e), Some(g)) => (e, g),
            _ => (Duration::from_millis(10), 2.0),
        };
        let request = SensorRequest {
            frame: f + 3,
            exposure: want.0,
            analogue_gain: want.1,
            frame_duration: Duration::from_millis(33),
        };
        landing.push((f + 3, want));
        let report = LoopReport {
            lands: Some(f + 3),
            request: Some(request),
            total_exposure: 0.02,
            ae_locked: true,
        };
        if let Some(o) = runner.after_frame(host, &sensor, &report, raw) {
            return Some((f, o));
        }
    }
    None
}

#[test]
fn a_bracket_lands_on_consecutive_frames_and_ae_gets_its_controls_back() {
    let mut runner = StillRunner::new();
    let mut host = FakeLoop::default();
    runner.submit(7, order(ShotExposure::Bracket(vec![-1.0, 0.0, 1.0])));
    let Some((_, StillOutcome::Taken { job, shots })) = run(&mut runner, &mut host, 20) else {
        panic!("no bracket")
    };
    assert_eq!(job, 7);
    // Started after frame 0, requested at frames 1, 2, 3: consecutive frames 4, 5, 6.
    let frames: Vec<u64> = shots.iter().map(|s| s.raw).collect();
    assert_eq!(frames, [4, 5, 6]);
    for (s, ev) in shots.iter().zip([-1.0, 0.0, 1.0]) {
        assert_eq!((s.ev, s.target), (ev, Some(s.raw)));
        let (e, g) = s.want.unwrap();
        let total = e.as_secs_f64() * g;
        assert!((total / (0.02 * 2f64.powf(ev)) - 1.0).abs() < 1e-6);
        assert!(s.landed(s.raw, &values(s.raw, e, g)));
    }
    // Three exposures, then AE's controls back.
    assert_eq!(host.controls.len(), 4);
    let last = host.controls.last().unwrap();
    assert_eq!((last.exposure, last.analogue_gain), (None, None));
    assert!(runner.targets().is_empty() && runner.is_idle());
}

#[test]
fn a_current_exposure_still_is_the_next_frame_and_requests_time_out() {
    let mut runner = StillRunner::new();
    let mut host = FakeLoop::default();
    runner.submit(1, order(ShotExposure::Current));
    let Some((f, StillOutcome::Taken { shots, .. })) = run(&mut runner, &mut host, 5) else {
        panic!()
    };
    // Starts before frame 1 (frame 0 said what AE does), held on frame 1.
    assert_eq!((f, shots.len(), shots[0].raw), (1, 1, 1));
    assert!(
        host.controls.is_empty(),
        "AE's exposure: controls untouched"
    );

    // A fixed exposure the sensor never gives (a loop that ignores it) times out after
    // GIVE_UP frames take whatever comes; a request whose timeout passed fails at once.
    let mut runner = StillRunner::new();
    let mut late = order(ShotExposure::Fixed {
        exposure: Duration::from_millis(20),
        gain: 1.0,
    });
    late.timeout = Duration::from_millis(10);
    runner.submit(2, late);
    let Some((_, StillOutcome::Failed { job, reason })) = run(&mut runner, &mut host, 5) else {
        panic!()
    };
    assert_eq!((job, reason), (2, StillFailure::TimedOut));
    // The application's controls were handed back.
    assert_eq!(host.controls.last().unwrap().exposure, None);

    // An empty bracket answers at once.
    let mut runner = StillRunner::new();
    runner.submit(3, order(ShotExposure::Bracket(Vec::new())));
    let Some((_, StillOutcome::Taken { shots, .. })) = run(&mut runner, &mut host, 3) else {
        panic!()
    };
    assert!(shots.is_empty());
}

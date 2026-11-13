//! Autofocus on the simulated backend: lemnosd in this process, a simulated `lens/1` driver
//! (`sim/mod.rs`), and a synthetic target whose contrast peaks at a known lens position. AF moves
//! the lens through the Lemnos client; the frames' statistics are computed from the position the
//! lens reports back.

mod sim;

use core::time::Duration;
use std::path::PathBuf;

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_aaa::{AfMode, AfState, Controls, FrameMetadata, Statistics, ZoneGrid};
use styx_controls::{Control, ControlValue};
use styx_focus::{Autofocus, AutofocusConfig, Focus};
use styx_lemnos::Client;

/// The target's peak: 5 dioptres, which the default AF map (0..12 D over the lens's range) puts at
/// code 426.25 on the 10-bit lens.
const PEAK_D: f64 = 5.0;
const PEAK_CODE: f64 = PEAK_D * 1023.0 / 12.0;
/// The contrast curve's half width, in dioptres (as styx-aaa's own synthetic scenes).
const WIDTH_D: f64 = 1.5;
/// Frames a run is allowed. Convergence takes far fewer.
const FRAMES: u64 = 400;
/// Lens moves a run may take: a coarse scan, a fine scan and a settle, with slack.
const MOVE_BOUND: u32 = 40;

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = std::env::temp_dir().join(format!("styx-focus-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{name}.sock"));
    let config = Config {
        socket: socket.clone(),
        name: "styx-focus-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![entry::<sim::SimLens>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// Contrast of the synthetic scene with the lens at `code`: the default map is a straight line
/// from 0 D at code 0 to 12 D at code 1023.
fn scene(code: u16) -> Statistics {
    let d = f64::from(code) * 12.0 / 1023.0;
    let v = 1.0 / (1.0 + ((d - PEAK_D) / WIDTH_D).powi(2));
    let mut focus = ZoneGrid::<f64>::new(8, 8);
    focus.zones.fill(v);
    Statistics {
        focus: Some(focus),
        ..Statistics::default()
    }
}

fn controls() -> Controls {
    Controls {
        af_mode: AfMode::Auto,
        af_trigger: 1,
        ..Controls::default()
    }
}

/// Runs AF for up to `FRAMES` frames over the simulated lens. Returns the last frame's result and
/// the lens's own position at the end.
fn run(af: &mut Autofocus, saw_fault: &mut bool) -> (Focus, Option<u16>) {
    let mut last = None;
    for frame in 0..FRAMES {
        let code = af.device_position().expect("position").unwrap_or(85);
        let mut meta = FrameMetadata::new(
            frame,
            Duration::from_millis(10),
            1.0,
            Duration::from_millis(33),
        );
        meta.controls = controls();
        last = Some(af.process(&scene(code), &mut meta));
        *saw_fault |= af.fault().is_some();
        if af.pending().is_some() {
            // The chip or the retry backoff needs real time, as on the board.
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    (
        last.expect("frames ran"),
        af.device_position().expect("position"),
    )
}

fn check_converged(focus: &Focus, final_code: Option<u16>, af: &Autofocus) {
    let code = f64::from(final_code.expect("the lens took a move"));
    let value = focus.value;
    println!(
        "final code {code}, focus value {value:?}, state {:?}, moves taken {}",
        focus.status.state,
        af.moves_taken()
    );
    assert_eq!(focus.status.state, AfState::Focused);
    assert!(
        (code - PEAK_CODE).abs() <= 12.0,
        "lens at {code}, peak at {PEAK_CODE:.2}"
    );
    assert!(
        af.moves_taken() <= MOVE_BOUND,
        "{} moves taken",
        af.moves_taken()
    );
    assert!(af.pending().is_none(), "a move is still pending");
    // 5 D is (12 - 5) / 12 of the way from the nearest end: about 583 on the 0..1000 scale.
    let value = value.expect("a Focus value");
    assert!(value.abs_diff(583) <= 10, "focus value {value}");
    assert_eq!(value, af.scale().value(code));
}

#[test]
fn focus_converges_on_the_peak_through_the_lens_contract() {
    let (service, socket) = start("converge", &sim::board(0));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");

    let mut af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    let mut saw_fault = false;
    let (focus, final_code) = run(&mut af, &mut saw_fault);
    check_converged(&focus, final_code, &af);
    assert!(!saw_fault, "no fault on the simulated lens");

    // The Focus value goes into the frame's metadata.
    let mut out = styx_controls::FrameMetadata::default();
    focus.stamp(&mut out);
    assert_eq!(
        out.controls[Control::Focus.index()],
        ControlValue::Manual(focus.value.expect("a Focus value"))
    );

    drop(af);
    drop(client);
    service.stop();
}

#[test]
fn a_lens_that_fails_its_moves_is_retried_and_still_converges() {
    let (service, socket) = start("retry", &sim::board(3));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");

    let mut af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    let mut saw_fault = false;
    let (focus, final_code) = run(&mut af, &mut saw_fault);
    assert!(saw_fault, "the failed moves were not seen");
    check_converged(&focus, final_code, &af);

    drop(af);
    drop(client);
    service.stop();
}

/// The lens's direction answer decides which way the Focus scale runs; `Unknown` keeps the AF map's
/// direction (rising in the default map).
#[test]
fn the_lens_direction_answer_sets_the_focus_scale_and_unknown_keeps_the_map() {
    // Unknown: the map's direction, rising. The nearest end (the top code) is Focus 0.
    let (service, socket) = start("dir-unknown", &sim::board_with(0, 0, None));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");
    let af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    assert_eq!((af.scale().value(0.0), af.scale().value(1023.0)), (1000, 0));
    drop(af);
    drop(client);
    service.stop();

    // Toward near: increasing positions focus nearer, so the scale rises.
    let (service, socket) = start("dir-near", &sim::board_with(0, 1, None));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");
    let af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    assert_eq!((af.scale().value(0.0), af.scale().value(1023.0)), (1000, 0));
    drop(af);
    drop(client);
    service.stop();

    // Toward far: the same map, the other way: the scale falls, so the bottom code is nearest.
    let (service, socket) = start("dir-far", &sim::board_with(0, 2, None));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");
    let af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    assert_eq!((af.scale().value(0.0), af.scale().value(1023.0)), (0, 1000));
    drop(af);
    drop(client);
    service.stop();
}

/// The lens's settle answer is used for each move when it gives one, and the configured settle
/// time when it does not.
#[test]
fn the_lens_settle_answer_is_used_per_move_and_the_config_is_the_fallback() {
    // The lens's own 100 ms at a 33 ms frame is 4 frames (rounded up).
    let (service, socket) = start("settle-given", &sim::board_with(0, 0, Some(100_000)));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");
    let mut af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    let mut saw_fault = false;
    run(&mut af, &mut saw_fault);
    assert_eq!(af.last_settle_frames(), Some(4));
    drop(af);
    drop(client);
    service.stop();

    // No figure: the configured 12 ms at 33 ms is one frame.
    let (service, socket) = start("settle-none", &sim::board_with(0, 0, None));
    let client = Client::connect(&socket, "styx-focus-test").expect("connect");
    let mut af = Autofocus::open(&client, "lens0", AutofocusConfig::default()).expect("open");
    let mut saw_fault = false;
    run(&mut af, &mut saw_fault);
    assert_eq!(af.last_settle_frames(), Some(1));
    drop(af);
    drop(client);
    service.stop();
}

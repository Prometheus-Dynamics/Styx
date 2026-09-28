//! The OV9782 description against the numbers of the kernel driver it was derived from, and the
//! driver runtime over a mock bus.

use std::sync::Arc;
use std::time::Duration;

use styx_sensor::{
    BusOp, ColorFilter, Control, ControlRequest, ControlSet, DriverState, MbusCode, MockBus,
    MockPins, PinOp, RegWrite, SensorDescription, SensorDriver, SensorError,
};

const OV9782: &str = include_str!("../sensors/ov9782.toml");

fn desc() -> SensorDescription {
    SensorDescription::from_toml_str(OV9782, "ov9782.toml").expect("ov9782.toml is valid")
}

// Constants of the kernel driver (continuous CSI-2 clock).
const LINK_FREQ: f64 = 400e6;
const LANES: f64 = 2.0;
const EXPOSURE_OFFSET: u32 = 12;
/// (width, height, hblank_min, vblank_default, vblank_min, vblank_max)
const MODES: [(u32, u32, u32, u32, u32, u32); 3] = [
    (1280, 800, 176, 1022, 110, 51540),
    (1280, 720, 176, 1022, 41, 51540),
    (640, 400, 816, 1022, 22, 51540),
];

fn driver_fps(bits: f64, w: u32, h: u32, hblank: u32, vblank: u32) -> f64 {
    let pixel_rate = LINK_FREQ * 2.0 * LANES / bits;
    pixel_rate / (f64::from(w + hblank) * f64::from(h + vblank))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9 * b.abs().max(1.0)
}

#[test]
fn identity_and_formats() {
    let d = desc();
    assert_eq!(d.sensor.name, "ov9782");
    assert_eq!(d.sensor.i2c_address, Some(0x60));
    assert_eq!(d.pixel_array.color_filter, ColorFilter::Bggr);
    assert_eq!(d.formats["raw10"].code, MbusCode::SBGGR10_1X10);
    assert_eq!(d.formats["raw8"].code, MbusCode::SBGGR8_1X8);
    assert_eq!(d.formats["raw10"].bits(), 10);
    // Pixel rate = link frequency x 2 x lanes / bpp, as the driver reports it.
    assert_eq!(
        d.formats["raw10"].pixel_rate as f64,
        LINK_FREQ * 2.0 * LANES / 10.0
    );
    assert_eq!(
        d.formats["raw8"].pixel_rate as f64,
        LINK_FREQ * 2.0 * LANES / 8.0
    );
    assert_eq!(
        d.modes.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        ["1280x800", "1280x720", "640x400"]
    );
    assert_eq!(
        d.color_filter(true, true),
        ColorFilter::Bggr,
        "flips do not change the code"
    );
}

#[test]
fn timing_reproduces_the_driver_frame_rates() {
    let d = desc();
    for (format, bits) in [("raw10", 10.0), ("raw8", 8.0)] {
        for (w, h, hb, vb_default, vb_min, vb_max) in MODES {
            let t = d.timing(&format!("{w}x{h}"), format).unwrap();
            assert_eq!(t.line_length(), w + hb);
            assert_eq!(t.frame_length_default(), h + vb_default);
            let default_fps = t.fps(t.frame_length_default());
            assert!(
                close(default_fps, driver_fps(bits, w, h, hb, vb_default)),
                "{format} {w}x{h}: {default_fps}"
            );
            let (lo, hi) = t.fps_range();
            assert!(
                close(hi, driver_fps(bits, w, h, hb, vb_min)),
                "{format} {w}x{h} max {hi}"
            );
            assert!(
                close(lo, driver_fps(bits, w, h, hb, vb_max)),
                "{format} {w}x{h} min {lo}"
            );
        }
    }
}

#[test]
fn computed_fps_ranges() {
    let d = desc();
    let r = |m: &str, f: &str| {
        let t = d.timing(m, f).unwrap();
        let (lo, hi) = t.fps_range();
        (lo, t.fps(t.frame_length_default()), hi)
    };
    // (mode, format, min fps, default fps, max fps)
    let expected = [
        ("1280x800", "raw10", 2.0995, 60.3129, 120.758),
        ("1280x720", "raw10", 2.1028, 63.0827, 144.402),
        ("640x400", "raw10", 2.1157, 77.2786, 260.403),
        ("1280x800", "raw8", 2.6244, 75.3911, 150.948),
        ("1280x720", "raw8", 2.6284, 78.8534, 180.503),
        ("640x400", "raw8", 2.6446, 96.5982, 325.504),
    ];
    for (m, f, lo, def, hi) in expected {
        let got = r(m, f);
        assert!(
            (got.0 - lo).abs() < 1e-4 && (got.1 - def).abs() < 1e-4 && (got.2 - hi).abs() < 1e-3,
            "{m} {f}: {got:?}"
        );
    }
}

#[test]
fn target_frame_rates_round_to_whole_lines() {
    let d = desc();
    let t = d.timing("1280x800", "raw10").unwrap();
    let f = t.frame_length_for_fps(30.0);
    assert_eq!(f.lines, 3663);
    assert_eq!(f.vblank, 2863);
    assert!((f.fps - 30.0).abs() < 1e-3);
    let f = t.frame_length_for_fps(120.0);
    assert_eq!(f.lines, 916);
    // Low light: let the frame rate drop from 60 to 10 fps.
    let (short, long) = t.frame_length_range(10.0, 60.0);
    assert!(short.fps <= 60.0 && long.fps >= 10.0);
    assert_eq!((short.lines, long.lines), (1832, 10989));
}

#[test]
fn exposure_limits_match_the_driver() {
    let d = desc();
    for (w, h, _, vb_default, vb_min, vb_max) in MODES {
        let t = d.timing(&format!("{w}x{h}"), "raw10").unwrap();
        for vb in [vb_min, vb_default, vb_max] {
            let l = t.exposure_limits(h + vb);
            assert_eq!(l.min_lines, 1.0);
            // The driver's EXPOSURE control maximum: vblank + height - OV9782_EXPOSURE_OFFSET.
            assert_eq!(l.max_lines, f64::from(h + vb - EXPOSURE_OFFSET));
        }
    }
    // The device reported EXPOSURE max 3638 at 1280x800 with VBLANK 2850.
    let t = d.timing("1280x800", "raw10").unwrap();
    assert_eq!(t.exposure_limits(800 + 2850).max_lines, 3638.0);
    assert_eq!(t.line_time(), Duration::from_nanos(9100));
    // Default exposure 642 lines at 10 bits.
    assert_eq!(t.lines_to_duration(642.0), Duration::from_nanos(642 * 9100));
}

#[test]
fn gain_matches_the_driver_range() {
    let d = desc();
    let g = &d.controls.analog_gain;
    assert_eq!(g.range(), (1.0, 255.0 / 16.0));
    let c = g.code_for_gain(4.0, styx_sensor::Rounding::Nearest);
    assert_eq!((c.code, c.gain), (0x40, 4.0));
    let s = styx_sensor::split_gain(g, d.controls.digital_gain.as_ref(), 3.3);
    assert_eq!(s.analog.code, 53);
    assert_eq!(s.total, 53.0 / 16.0);
}

fn powered() -> SensorDriver<MockBus, MockPins> {
    let bus = MockBus::new().with_register(0x300a, 2, 0x9782);
    let mut drv = SensorDriver::new(
        Arc::new(desc()),
        bus,
        MockPins::with_roles(&["avdd", "xvclk"]),
    );
    drv.power_up().unwrap();
    drv
}

#[test]
fn power_up_skips_optional_roles_the_board_lacks() {
    let drv = powered();
    assert_eq!(
        drv.pins().log,
        vec![
            PinOp::Supply("avdd".into(), true),
            PinOp::Delay(Duration::from_micros(600)),
            PinOp::Clock("xvclk".into(), Some(24_000_000)),
            PinOp::Delay(Duration::from_micros(600)),
        ]
    );
    assert_eq!(drv.bus().writes(), vec![RegWrite::byte(0x4800, 0x00)]);
    assert_eq!(drv.state(), DriverState::Powered);
}

#[test]
fn chip_id_is_checked() {
    let mut drv = powered();
    assert_eq!(drv.verify_chip_id().unwrap(), 0x9782);
    // Some modules report the OV9281 id; the driver accepts it.
    drv.bus_mut().registers.insert(0x300a, 0x92);
    drv.bus_mut().registers.insert(0x300b, 0x81);
    assert_eq!(drv.verify_chip_id().unwrap(), 0x9281);
    drv.bus_mut().registers.insert(0x300a, 0x12);
    let e = drv.verify_chip_id().unwrap_err();
    assert!(
        matches!(e, SensorError::ChipId { found: 0x1281, .. }),
        "{e}"
    );
}

#[test]
fn bring_up_writes_what_the_kernel_driver_writes() {
    let d = desc();
    let mut drv = powered();
    drv.bus_mut().clear_log();
    drv.init().unwrap();
    assert_eq!(drv.bus().writes().len(), 61);
    drv.bus_mut().clear_log();
    let mode = drv.set_mode("1280x800", "raw10").unwrap();
    assert_eq!(mode.code, MbusCode::SBGGR10_1X10);
    let writes = drv.bus().writes();
    let mode_regs = d.mode("1280x800").unwrap().registers.len();
    assert_eq!(
        writes[..2],
        [RegWrite::byte(0x030d, 0x50), RegWrite::byte(0x3662, 0x05)]
    );
    assert_eq!(
        writes[2 + mode_regs..],
        [
            RegWrite {
                address: 0x380c,
                value: 1456 / 2,
                bytes: 2
            },
            RegWrite {
                address: 0x380e,
                value: 1822,
                bytes: 2
            },
            RegWrite {
                address: 0x3500,
                value: 642 << 4,
                bytes: 3
            },
            RegWrite {
                address: 0x3509,
                value: 0x10,
                bytes: 1
            },
            RegWrite::byte(0x3821, 0x04),
            RegWrite::byte(0x3820, 0x44),
        ]
    );
    // Flips are read-modify-write.
    assert!(drv.bus().log.contains(&BusOp::Read {
        address: 0x3820,
        bytes: 1,
        value: 0x40
    }));
    drv.bus_mut().clear_log();
    drv.start_streaming().unwrap();
    assert_eq!(drv.bus().writes(), vec![RegWrite::byte(0x0100, 0x01)]);
    assert_eq!(drv.state(), DriverState::Streaming);
}

#[test]
fn raw8_mode_writes_the_raw8_registers() {
    let mut drv = powered();
    drv.init().unwrap();
    drv.bus_mut().clear_log();
    drv.set_mode("640x400", "raw8").unwrap();
    assert_eq!(
        drv.bus().writes()[..2],
        [RegWrite::byte(0x030d, 0x60), RegWrite::byte(0x3662, 0x07)]
    );
    assert_eq!(drv.bus().value(0x380c, 2), 1456 / 2);
    assert!(matches!(
        drv.set_mode("640x480", "raw8"),
        Err(SensorError::UnknownMode(_))
    ));
}

#[test]
fn scheduled_controls_are_written_in_group_hold_on_time() {
    let mut drv = powered();
    drv.init().unwrap();
    drv.set_mode("1280x800", "raw10").unwrap();
    drv.start_streaming().unwrap();
    drv.bus_mut().clear_log();
    let req = ControlRequest {
        exposure: Some(Duration::from_millis(10)),
        gain: Some(2.0),
        frame_duration: Some(Duration::from_secs_f64(1.0 / 30.0)),
    };
    let landings = drv.request(10, &req).unwrap();
    assert!(landings.iter().all(|l| l.frame == 10));
    for f in 0..8 {
        assert!(drv.frame_start(f).unwrap().controls.is_empty());
    }
    let batch = drv.frame_start(8).unwrap();
    assert_eq!(
        batch.controls,
        ControlSet::new()
            .with(Control::FrameLength, 3663)
            .with(Control::Exposure, 1099)
            .with(Control::AnalogGain, 0x20)
    );
    assert_eq!(
        drv.bus().writes(),
        vec![
            RegWrite::byte(0x3308, 0x01),
            RegWrite {
                address: 0x380e,
                value: 3663,
                bytes: 2
            },
            RegWrite {
                address: 0x3500,
                value: 1099 << 4,
                bytes: 3
            },
            RegWrite {
                address: 0x3509,
                value: 0x20,
                bytes: 1
            },
            RegWrite::byte(0x3308, 0x00),
        ]
    );
    let before = drv.applied(9).unwrap();
    assert_eq!(before.exposure_lines, 642.0);
    let a = drv.applied(10).unwrap();
    assert_eq!(a.exposure_lines, 1099.0);
    assert_eq!(a.exposure, Duration::from_nanos(1099 * 9100));
    assert_eq!(a.analog_gain, 2.0);
    assert_eq!(a.frame_length, 3663);
    assert!((1.0 / a.frame_duration.as_secs_f64() - 30.0).abs() < 1e-3);
}

#[test]
fn stop_writes_pending_values_and_restarts_numbering() {
    let mut drv = powered();
    drv.init().unwrap();
    drv.set_mode("1280x720", "raw10").unwrap();
    drv.start_streaming().unwrap();
    drv.frame_start(0).unwrap();
    drv.request(
        50,
        &ControlRequest {
            gain: Some(4.0),
            ..Default::default()
        },
    )
    .unwrap();
    drv.bus_mut().clear_log();
    drv.stop_streaming().unwrap();
    assert_eq!(drv.bus().writes()[0], RegWrite::byte(0x0100, 0x00));
    assert_eq!(drv.bus().value(0x3509, 1), 0x40);
    assert_eq!(drv.applied(0).unwrap().analog_gain, 4.0);
    assert!(drv.frame_start(1).is_err());
    drv.power_down().unwrap();
    assert_eq!(drv.state(), DriverState::Off);
}

#[test]
fn requests_before_streaming_apply_to_the_first_frame() {
    let mut drv = powered();
    drv.init().unwrap();
    drv.set_mode("1280x800", "raw10").unwrap();
    drv.request(
        0,
        &ControlRequest {
            exposure: Some(Duration::from_millis(5)),
            ..Default::default()
        },
    )
    .unwrap();
    drv.bus_mut().clear_log();
    drv.start_streaming().unwrap();
    let w = drv.bus().writes();
    assert_eq!(
        w[0],
        RegWrite {
            address: 0x3500,
            value: 549 << 4,
            bytes: 3
        }
    );
    assert_eq!(w[1], RegWrite::byte(0x0100, 0x01));
    assert_eq!(drv.applied(0).unwrap().exposure_lines, 549.0);
}

#[test]
fn state_is_enforced() {
    let mut drv = SensorDriver::new(Arc::new(desc()), MockBus::new(), MockPins::default());
    assert!(matches!(drv.init(), Err(SensorError::State(_))));
    assert!(matches!(drv.start_streaming(), Err(SensorError::State(_))));
    drv.power_up().unwrap();
    assert!(matches!(drv.start_streaming(), Err(SensorError::State(_))));
    assert!(matches!(
        drv.set_test_pattern("bars"),
        Err(SensorError::NoRegister(_))
    ));
}

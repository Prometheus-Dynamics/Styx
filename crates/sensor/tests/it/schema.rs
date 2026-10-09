//! Schema parsing and validation messages, embedded data, and kernel-driven descriptions.

use std::sync::Arc;

use styx_sensor::{
    Backend, Control, ControlSet, GainModel, MockBus, MockPins, RegWrite, SensorDescription,
    SensorDriver, SensorError,
};

const BASE: &str = r#"
[sensor]
name = "test"
i2c_address = 0x36
chip_id = { address = 0x300a, bytes = 2, values = [0x5678] }
clocks = { xclk = 24_000_000 }

[pixel_array]
size = [1000, 800]
active = { left = 0, top = 0, width = 1000, height = 800 }
color_filter = "mono"

[sequences]
power_up = [{ clock = "xclk", on = true }]
stream_on = [[0x0100, 1]]

[formats.y10]
code = "Y10_1X10"
pixel_rate = 100_000_000

[[modes]]
name = "full"
size = [1000, 800]
crop = { left = 0, top = 0, width = 1000, height = 800 }
hblank = { min = 200, max = 2000, default = 200 }
vblank = { min = 20, max = 10000, default = 200 }

[controls]
frame_length = { address = 0x380e, bytes = 2 }

[controls.exposure]
register = { address = 0x3500, bytes = 2, shift = 4, bits = 12 }
fraction_bits = 4
min = 1
margin = 8
default = 100

[controls.analog_gain]
register = { address = 0x3509 }
min_code = 0
max_code = 3
default_code = 0
model = { table = [[0, 1.0], [1, 2.0], [2, 4.0], [3, 8.0]] }

[controls.digital_gain]
register = { address = 0x350a, bytes = 2 }
min_code = 256
max_code = 4095
default_code = 256
model = { linear = { step = 0.00390625 } }

[controls.test_pattern]
register = { address = 0x5e00, shift = 7, bits = 1, read_modify_write = true }
patterns = { off = 0, bars = 1 }

[embedded_data]
lines = 2
entries = [
    { address = 0x3500, offset = 10 }, { address = 0x3501, offset = 11 },
    { address = 0x3509, offset = 12 },
    { address = 0x380e, offset = 20 }, { address = 0x380f, offset = 21 },
]
"#;

fn parse(src: &str) -> Result<SensorDescription, SensorError> {
    SensorDescription::from_toml_str(src, "test.toml")
}

fn err(src: &str) -> String {
    parse(src).unwrap_err().to_string()
}

#[test]
fn the_base_description_is_valid() {
    let d = parse(BASE).unwrap();
    assert_eq!(d.sensor.backend, Backend::Registers);
    assert_eq!(d.sensor.address_bits, 16);
    assert!(matches!(d.controls.analog_gain.model, GainModel::Table(_)));
    assert_eq!(d.controls.delays, styx_sensor::Delays::default());
}

#[test]
fn unknown_fields_are_rejected_with_their_location() {
    let e = err(&BASE.replace("name = \"test\"", "name = \"test\"\nnmae = 1"));
    assert!(
        e.contains("unknown field `nmae`") && e.contains("line"),
        "{e}"
    );
    let e = err(&BASE.replace("step = 0.00390625", "stepp = 0.00390625"));
    assert!(
        e.contains("stepp") && e.contains("available keys: step, offset"),
        "{e}"
    );
    let e = err(&BASE.replace("color_filter = \"mono\"", "color_filter = \"RGBW\""));
    assert!(e.contains("RGBW"), "{e}");
    let e = err(&BASE.replace("Y10_1X10", "Y11_1X11"));
    assert!(e.contains("unknown media bus code 'Y11_1X11'"), "{e}");
}

#[test]
fn every_problem_is_reported_with_its_path() {
    let src = BASE
        .replace("i2c_address = 0x36", "i2c_address = 0x80")
        .replace(
            "crop = { left = 0, top = 0, width = 1000, height = 800 }",
            "crop = { left = 8, top = 0, width = 1000, height = 800 }",
        )
        .replace(
            "vblank = { min = 20, max = 10000, default = 200 }",
            "vblank = { min = 300, max = 10000, default = 200 }",
        )
        .replace("stream_on = [[0x0100, 1]]", "stream_on = [[0x0100, 0x100]]")
        .replace(
            "{ clock = \"xclk\", on = true }",
            "{ clock = \"mclk\", on = true }",
        )
        .replace("default_code = 0\n", "default_code = 9\n");
    let e = parse(&src).unwrap_err();
    let SensorError::Invalid { issues, .. } = &e else {
        panic!("{e}")
    };
    let text = e.to_string();
    for needle in [
        "sensor.i2c_address: 0x80 is not a 7-bit I2C device address",
        "modes[0] (full).crop: must lie inside pixel_array.size",
        "modes[0] (full).vblank: needs min <= default <= max, got 300 / 200 / 10000",
        "sequences.stream_on[0]: value 0x100 does not fit 1 byte(s)",
        "sequences.power_up[0]: clock 'mclk' is not declared in sensor.clocks",
        "controls.analog_gain: codes must satisfy min_code <= default_code <= max_code",
    ] {
        assert!(text.contains(needle), "missing '{needle}' in:\n{text}");
    }
    assert_eq!(issues.0.len(), 6, "{text}");
}

#[test]
fn register_widths_and_fields_are_checked() {
    let e = err(&BASE.replace("[sensor]\n", "[sensor]\naddress_bits = 8\n"));
    assert!(e.contains("does not fit an address of 0xff max"), "{e}");
    let e = err(&BASE.replace("shift = 4, bits = 12", "shift = 4, bits = 16"));
    assert!(
        e.contains("shift 4 + bits 16 exceed the 16-bit register"),
        "{e}"
    );
    let e = err(&BASE.replace("fraction_bits = 4", "fraction_bits = 5"));
    assert!(e.contains("fraction_bits <= shift"), "{e}");
    let e = err(&BASE.replace(
        "patterns = { off = 0, bars = 1 }",
        "patterns = { bars = 2 }",
    ));
    assert!(
        e.contains("must include `off`") && e.contains("does not fit the register field"),
        "{e}"
    );
    let e = err(&BASE.replace("[[0, 1.0], [1, 2.0]", "[[0, 1.0], [1, 0.5]"));
    assert!(e.contains("gain table must increase"), "{e}");
    let e = err(&BASE.replace("pixel_rate = 100_000_000", "pixel_rate = 0"));
    assert!(
        e.contains("formats.y10: pixel_rate must be positive"),
        "{e}"
    );
    let e = err(&BASE.replace("color_filter = \"mono\"", "color_filter = \"RGGB\""));
    assert!(e.contains("does not match pixel_array.color_filter"), "{e}");
}

#[test]
fn modes_refer_to_known_formats_and_have_unique_names() {
    let extra = r#"
[[modes]]
name = "full"
size = [500, 400]
crop = { left = 0, top = 0, width = 1000, height = 800 }
formats = ["y12"]
binning = [2, 2]
hblank = { min = 200, max = 2000, default = 200 }
vblank = { min = 20, max = 10000, default = 200 }
"#;
    let src = BASE.replacen("[controls]", &format!("{extra}\n[controls]"), 1);
    let e = err(&src);
    assert!(e.contains("modes[1] (full): duplicate mode name"), "{e}");
    assert!(e.contains("unknown format 'y12'"), "{e}");
    let src = src
        .replace(
            "name = \"full\"\nsize = [500, 400]",
            "name = \"half\"\nsize = [600, 400]",
        )
        .replace("[\"y12\"]", "[\"y10\"]");
    let e = err(&src);
    assert!(
        e.contains("width 600 x binning/skipping 2 exceeds the crop width 1000"),
        "{e}"
    );
}

#[test]
fn registers_backend_needs_registers() {
    let e = err(&BASE.replace("frame_length = { address = 0x380e, bytes = 2 }", ""));
    assert!(
        e.contains("controls.frame_length: a register-driven sensor needs the frame length"),
        "{e}"
    );
    let kernel = BASE
        .replace("[sensor]\n", "[sensor]\nbackend = \"kernel\"\n")
        .replace("frame_length = { address = 0x380e, bytes = 2 }", "")
        .replace("register = { address = 0x3509 }\n", "");
    let d = parse(&kernel).unwrap();
    assert_eq!(d.sensor.backend, Backend::Kernel);
    // A kernel-driven description runs over the bus's V4L2 controls, not its registers.
    let mut drv = SensorDriver::new(Arc::new(d), MockBus::new(), MockPins::with_roles(&["xclk"]));
    drv.power_up().unwrap();
    drv.set_mode("full", "y10").unwrap();
    assert!(drv.bus().writes().is_empty());
    assert_eq!(
        drv.bus().control(styx_sensor::KernelControl::Vblank),
        Some(200)
    );
}

#[test]
fn embedded_data_decodes_to_scheduler_codes() {
    let d = parse(BASE).unwrap();
    let mut data = vec![0u8; 32];
    // Exposure register 0x3500..0x3501 = 0x0648: integer 100 lines, fraction 8/16.
    data[10] = 0x06;
    data[11] = 0x48;
    data[12] = 2;
    data[20] = 0x04;
    data[21] = 0x4c;
    let codes = d.decode_embedded(&data);
    assert_eq!(
        codes,
        ControlSet::new()
            .with(Control::FrameLength, 1100)
            .with(Control::Exposure, 0x648)
            .with(Control::AnalogGain, 2)
    );
    // Too short: only what is present is decoded.
    assert_eq!(
        d.decode_embedded(&data[..13]).get(Control::FrameLength),
        None
    );
}

#[test]
fn driver_writes_fractional_exposure_digital_gain_and_test_pattern() {
    let d = Arc::new(parse(BASE).unwrap());
    let bus = MockBus::new()
        .with_register(0x300a, 2, 0x5678)
        .with_register(0x5e00, 1, 0x05);
    let mut drv = SensorDriver::new(d, bus, MockPins::with_roles(&["xclk"]));
    drv.power_up().unwrap();
    drv.verify_chip_id().unwrap();
    drv.set_mode("full", "y10").unwrap();
    // 100 lines default, fraction bits 4: written as (100 << 4) << 0 into 0x3500 (shift 4 - 4).
    assert!(drv.bus().writes().contains(&RegWrite {
        address: 0x3500,
        value: 1600,
        bytes: 2
    }));
    drv.start_streaming().unwrap();
    drv.bus_mut().clear_log();
    let req = styx_sensor::ControlRequest {
        gain: Some(6.0),
        ..Default::default()
    };
    drv.request(0, &req).unwrap();
    let b = drv.issue_now().unwrap();
    // Analogue rounds down to 4x, digital makes up 1.5x.
    assert_eq!(b.controls.get(Control::AnalogGain), Some(2));
    assert_eq!(b.controls.get(Control::DigitalGain), Some(384));
    let a = drv.applied(b.lands(Control::AnalogGain).unwrap()).unwrap();
    assert_eq!(a.analog_gain * a.digital_gain, 6.0);
    drv.set_test_pattern("bars").unwrap();
    assert_eq!(drv.bus().value(0x5e00, 1), 0x85);
    assert!(matches!(
        drv.set_test_pattern("nope"),
        Err(SensorError::UnknownTestPattern(_))
    ));
}

#[test]
fn missing_files_are_reported() {
    let e = SensorDescription::from_file("/nonexistent/sensor.toml").unwrap_err();
    assert!(
        e.to_string()
            .starts_with("reading /nonexistent/sensor.toml"),
        "{e}"
    );
}

#[test]
fn embedded_controls_in_raw10_packing() {
    use styx_sensor::embedded_unpack_raw10;
    let src = BASE.replace(
        "[embedded_data]\nlines = 2\n",
        "[embedded_data]\nlines = 2\npacking = \"raw10\"\ncontrols = [\n    { control = \"exposure\", offset = 6, bytes = 2, shift = 4 },\n    { control = \"frame_length\", offset = 19, bytes = 2 },\n]\n",
    );
    let d = parse(&src).unwrap();
    // Words (one byte value each): 6..7 = 0x0282 (642 lines), 19..20 = 0x0e4f.
    let mut words = [0u8; 24];
    words[6] = 0x02;
    words[7] = 0x82;
    words[19] = 0x0e;
    words[20] = 0x4f;
    let packed: Vec<u8> = words
        .chunks(4)
        .flat_map(|w| {
            let low = w
                .iter()
                .enumerate()
                .fold(0u8, |a, (i, v)| a | ((v & 3) << (2 * i)));
            w.iter().map(|v| v >> 2).chain([low]).collect::<Vec<u8>>()
        })
        .collect();
    assert_eq!(embedded_unpack_raw10(&packed), words);
    let codes = d.decode_embedded(&packed);
    assert_eq!(codes.get(Control::Exposure), Some(642 << 4));
    assert_eq!(codes.get(Control::FrameLength), Some(0x0e4f));
    // A whole line (kilobytes) decodes the same: only the bytes the layout reads are used.
    let mut line = packed.clone();
    line.resize(16384, 0xa5);
    let long = d.decode_embedded(&line);
    assert_eq!(long.get(Control::Exposure), codes.get(Control::Exposure));
    assert_eq!(
        long.get(Control::FrameLength),
        codes.get(Control::FrameLength)
    );
    let bad = BASE.replace(
        "[embedded_data]\nlines = 2\n",
        "[embedded_data]\nlines = 2\ncontrols = [{ control = \"exposure\", offset = 0, bytes = 5 }]\n",
    );
    assert!(
        err(&bad).contains("embedded_data.controls[0]"),
        "{}",
        err(&bad)
    );
}

#[test]
fn the_bus_section_describes_the_wiring() {
    let d = parse(&format!(
        "{BASE}\n[bus]\nparallel = {{ width = 8, vsync_active_high = true }}\n"
    ))
    .unwrap();
    assert_eq!(
        d.bus.unwrap().to_hal(None),
        Some(styx_sensor::styx_hal::Bus::Parallel {
            width: 8,
            pclk_rising: true,
            hsync_active_high: true,
            vsync_active_high: true,
            embedded_sync: false,
        })
    );
    let d = parse(&format!("{BASE}\n[bus]\ncsi2 = {{ lanes = 2 }}\n")).unwrap();
    assert_eq!(
        d.bus.unwrap().to_hal(Some(400_000_000)),
        Some(styx_sensor::styx_hal::Bus::Csi2 {
            lanes: 2,
            link_frequency: 400_000_000,
            continuous_clock: true,
            virtual_channel: 0,
        })
    );
    assert!(parse(BASE).unwrap().bus.is_none());
    let e = err(&format!(
        "{BASE}\n[bus]\ncsi2 = {{ lanes = 5 }}\nparallel = {{ width = 4 }}\n"
    ));
    for problem in ["bus: give either", "bus.csi2.lanes", "bus.parallel.width"] {
        assert!(e.contains(problem), "{e}");
    }
    assert!(err(&format!("{BASE}\n[bus]\n")).contains("bus: needs parallel or csi2"));
    assert!(err(&format!("{BASE}\n[bus]\nusb = true\n")).contains("unknown field"));
}

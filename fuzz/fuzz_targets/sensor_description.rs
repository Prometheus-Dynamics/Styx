//! Sensor descriptions (TOML files on the search path) and everything run from them: the
//! timing and gain models, the driver over a mock register bus (power up, init, modes, flips,
//! test patterns, scheduled controls) and embedded-data decoding.
//!
//! Input: the TOML text, a NUL byte, then control requests and an embedded data line.

#![no_main]

use std::sync::Arc;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use styx_sensor::{
    ControlRequest, MockBus, MockPins, Rounding, SensorDescription, SensorDriver, Step, split_gain,
};

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fuzz_target!(|data: &[u8]| {
    let (text, rest) = match data.iter().position(|&b| b == 0) {
        Some(at) => (&data[..at], &data[at + 1..]),
        None => (data, &[][..]),
    };
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    let Ok(desc) = SensorDescription::from_toml_str(text, "fuzz.toml") else {
        return;
    };
    let exposure = Duration::from_micros(u64::from(u32_at(rest, 0)));
    let gain = f64::from(u32_at(rest, 4)) / 256.0;
    let fps = f64::from(u32_at(rest, 8)) / 16.0;
    let line = rest.get(12..).unwrap_or_default();

    for g in [
        Some(&desc.controls.analog_gain),
        desc.controls.digital_gain.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let (lo, hi) = g.range();
        for r in [Rounding::Nearest, Rounding::Down] {
            for v in [gain, lo, hi, 0.0, -1.0, f64::NAN, f64::INFINITY] {
                let _ = g.code_for_gain(v, r);
            }
        }
        let _ = g.gain_for_code(u32_at(rest, 4));
    }
    let _ = split_gain(
        &desc.controls.analog_gain,
        desc.controls.digital_gain.as_ref(),
        gain,
    );
    let _ = (
        desc.color_filter(true, false),
        desc.color_filter(false, true),
    );
    let _ = desc.decode_embedded(line);
    let _ = desc.embedded_registers(line);

    for mode in &desc.modes {
        for (format, _) in desc.formats_of(mode) {
            let Ok(t) = desc.timing(&mode.name, format) else {
                continue;
            };
            let _ = (t.fps_range(), t.line_time(), t.frame_length_default());
            for f in [fps, 0.0, 1.0, 30.0, 1e9, f64::NAN, -1.0] {
                let fl = t.frame_length_for_fps(f);
                let _ = t.exposure(exposure, fl.lines);
                let _ = t.exposure_limits(fl.lines);
            }
            let _ = t.frame_length_range(fps, 30.0);
            let _ = t.frame_length_for_duration(exposure);
            let _ = t.with_hblank(u32_at(rest, 8)).line_length();
            let _ = t.exposure_code_to_lines(u32_at(rest, 0));
            let _ = t.duration_to_lines(exposure);
            let _ = t.lines_to_duration(f64::from(u32_at(rest, 0)));
        }
    }

    // The driver over a mock bus, as `styx-native` runs it.
    let seq = &desc.sequences;
    let roles: Vec<&str> = seq
        .power_up
        .iter()
        .chain(&seq.power_down)
        .filter_map(|s| match s {
            Step::Gpio { role, .. } | Step::Clock { role, .. } | Step::Supply { role, .. } => {
                Some(role.as_str())
            }
            _ => None,
        })
        .collect();
    let pins = MockPins::with_roles(&roles);
    let mut driver = SensorDriver::new(Arc::new(desc.clone()), MockBus::new(), pins);
    if driver.power_up().is_err() || driver.init().is_err() {
        return;
    }
    let Some((mode, format)) = desc.modes.iter().find_map(|m| {
        desc.formats_of(m)
            .next()
            .map(|(f, _)| (m.name.clone(), f.to_owned()))
    }) else {
        return;
    };
    if driver.set_mode(&mode, &format).is_err() {
        return;
    }
    let _ = driver.set_flips(true, true);
    if let Some(p) = desc.controls.test_pattern.as_ref()
        && let Some(name) = p.patterns.keys().next()
    {
        let _ = driver.set_test_pattern(name);
    }
    let _ = driver.set_hblank(u32_at(rest, 8));
    let _ = driver.start_streaming();
    let req = ControlRequest {
        exposure: Some(exposure),
        gain: Some(gain),
        frame_duration: (fps > 0.0 && fps.is_finite())
            .then(|| Duration::from_secs_f64(1.0 / fps.max(1e-3))),
    };
    for frame in 0..4u64 {
        let _ = driver.request(frame + 2, &req);
        let _ = driver.frame_start(frame);
        let codes = desc.decode_embedded(line);
        let _ = driver.report(frame, &codes);
        let _ = driver.applied(frame);
    }
    let _ = driver.stop_streaming();
    let _ = driver.power_down();
});

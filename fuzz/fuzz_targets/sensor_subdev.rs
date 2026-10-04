//! Descriptions of sensors with a kernel driver, built from what the driver reports (sizes,
//! codes, control ranges, menus) and a kernel data file (TOML), then the timing and gain
//! models and the embedded data layout run from them.

#![no_main]

use std::collections::BTreeMap;
use std::time::Duration;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use styx_sensor::{
    BUILTIN_KERNEL_DATA, ControlRange, KernelControl, KernelSensorData, MbusCode, Rect, Rounding,
    SensorDescription, Size, SubdevFormat, SubdevReport,
};

#[derive(Arbitrary, Debug)]
struct Range {
    cid: u8,
    min: i64,
    max: i64,
    step: u64,
    default: i64,
    value: Option<i64>,
}

#[derive(Arbitrary, Debug)]
struct Input {
    name: String,
    formats: Vec<(u32, Vec<(u32, u32)>)>,
    native_size: Option<(u32, u32)>,
    crop_bounds: Option<(u32, u32, u32, u32)>,
    current_size: Option<(u32, u32)>,
    controls: Vec<Range>,
    link_frequencies: Vec<i64>,
    analogue_gain_unity: Option<i64>,
    flips_modify_layout: bool,
    test_patterns: Vec<(u32, String)>,
    /// A built-in data file, or `data` as one.
    builtin: Option<u8>,
    data: String,
    line: Vec<u8>,
    exposure_us: u32,
    gain: f64,
}

const CONTROLS: [KernelControl; 10] = [
    KernelControl::Exposure,
    KernelControl::AnalogueGain,
    KernelControl::DigitalGain,
    KernelControl::Vblank,
    KernelControl::Hblank,
    KernelControl::PixelRate,
    KernelControl::LinkFreq,
    KernelControl::HFlip,
    KernelControl::VFlip,
    KernelControl::TestPattern,
];

fn size((width, height): (u32, u32)) -> Size {
    Size { width, height }
}

fuzz_target!(|input: Input| {
    let report = SubdevReport {
        name: input.name,
        formats: input
            .formats
            .into_iter()
            .take(8)
            .map(|(code, sizes)| SubdevFormat {
                code: MbusCode(code),
                sizes: sizes.into_iter().take(8).map(size).collect(),
            })
            .collect(),
        native_size: input.native_size.map(size),
        crop_bounds: input.crop_bounds.map(|(left, top, width, height)| Rect {
            left,
            top,
            width,
            height,
        }),
        current_size: input.current_size.map(size),
        controls: input
            .controls
            .iter()
            .map(|r| {
                (
                    CONTROLS[usize::from(r.cid) % CONTROLS.len()],
                    ControlRange {
                        min: r.min,
                        max: r.max,
                        step: r.step,
                        default: r.default,
                        value: r.value,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        link_frequencies: input.link_frequencies,
        analogue_gain_unity: input.analogue_gain_unity,
        flips_modify_layout: input.flips_modify_layout,
        test_patterns: input.test_patterns,
    };
    let data = match input.builtin {
        Some(i) => {
            let (name, src) = BUILTIN_KERNEL_DATA[usize::from(i) % BUILTIN_KERNEL_DATA.len()];
            KernelSensorData::from_toml_str(src, name).ok()
        }
        None => KernelSensorData::from_toml_str(&input.data, "fuzz.toml").ok(),
    };
    if let Some(d) = &data {
        let _ = d.matches(&report.name, true);
    }
    let Ok(desc) = SensorDescription::from_subdev_with(&report, data.as_ref()) else {
        return;
    };
    let _ = desc.validate();
    let _ = desc.decode_embedded(&input.line);
    let (lo, hi) = desc.controls.analog_gain.range();
    for g in [input.gain, lo, hi] {
        let _ = desc
            .controls
            .analog_gain
            .code_for_gain(g, Rounding::Nearest);
    }
    let exposure = Duration::from_micros(u64::from(input.exposure_us));
    for mode in &desc.modes {
        for (format, _) in desc.formats_of(mode) {
            if let Ok(t) = desc.timing(&mode.name, format) {
                let fl = t.frame_length_for_fps(30.0);
                let _ = (t.fps_range(), t.exposure(exposure, fl.lines));
                let _ = t.frame_length_range(1.0, 120.0);
            }
        }
    }
});

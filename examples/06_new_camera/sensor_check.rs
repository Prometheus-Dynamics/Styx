//! Checks a new camera's data before it goes on a board: a sensor description (a sensor Styx
//! drives through its sensor bridge) or a kernel-driver data file (`*.kernel.toml`, a sensor
//! with its own kernel driver). Parses and validates it (every problem, with its path), then
//! prints what Styx will make of it: the modes and formats, the exact frame rate range of each,
//! the frame length and exposure range at common rates, the gain range and the control delays.
//! No hardware needed.
//!
//! ```sh
//! cargo run -p styx-examples --features native --bin sensor_check -- examples/06_new_camera/example_sensor.toml
//! cargo run -p styx-examples --features native --bin sensor_check -- examples/06_new_camera/example_sensor.kernel.toml
//! cargo run -p styx-examples --features native --bin sensor_check            # the built-in ones
//! ```
//!
//! On the board, next: `list_cameras` (the camera shows up with these modes), then
//! `native-pipeline regcheck` (tools/native-pipeline: every register written reads back, bridged
//! sensors) and `landing [fps] [rounds]` (crates/native/examples: exposure, gain and frame length
//! changes land on the frames the delays predict, from the image levels alone).

use styx_sensor::{KernelSensorData, SensorDescription};

fn check_description(desc: &SensorDescription) {
    let s = &desc.sensor;
    println!(
        "sensor {} ({}), I2C address {}, {}-bit register addresses, chip id {}",
        s.name,
        s.vendor.as_deref().unwrap_or("vendor not given"),
        s.i2c_address.map_or("-".into(), |a| format!("{a:#04x}")),
        s.address_bits,
        s.chip_id.as_ref().map_or("-".into(), |c| {
            let ids: Vec<String> = c.values.iter().map(|v| format!("{v:#06x}")).collect();
            format!("{} at {:#06x}", ids.join(" or "), c.address)
        })
    );
    let pa = &desc.pixel_array;
    println!(
        "pixel array {}x{}, active {}x{}, {:?}, black level {:?}",
        pa.size.width,
        pa.size.height,
        pa.active.width,
        pa.active.height,
        pa.color_filter,
        pa.black_level.map(|b| (b.value, b.bits))
    );
    let g = &desc.controls.analog_gain;
    println!(
        "analogue gain {:.2}x..{:.2}x; delays (frames) exposure {}, gain {}, frame length {}; \
         group hold {}; embedded data {}",
        g.gain_for_code(g.min_code),
        g.gain_for_code(g.max_code),
        desc.controls.delays.exposure,
        desc.controls.delays.analog_gain,
        desc.controls.delays.frame_length,
        if desc.controls.group_hold.is_some() {
            "yes"
        } else {
            "no"
        },
        desc.embedded_data
            .as_ref()
            .map_or("no".into(), |e| format!("{} line(s)", e.lines))
    );
    for mode in &desc.modes {
        for (name, format) in desc.formats_of(mode) {
            let Ok(t) = desc.timing(&mode.name, name) else {
                continue;
            };
            let (lo, hi) = t.fps_range();
            let default = t.fps(t.frame_length_default());
            println!(
                "mode {} {}x{} {name} ({}-bit, {:.1} MHz pixel rate): {lo:.3}..{hi:.3} fps, default {default:.3}",
                mode.name,
                mode.size.width,
                mode.size.height,
                format.bits(),
                format.pixel_rate as f64 / 1e6,
            );
            for fps in [15.0, 30.0, 60.0, 120.0] {
                if !(lo..=hi).contains(&fps) {
                    continue;
                }
                let fl = t.frame_length_for_fps(fps);
                let e = t.exposure_limits(fl.lines);
                println!(
                    "    {fps:>5} fps: VTS {} (vblank {}) -> {:.4} fps; exposure {:.3}..{:.3} ms",
                    fl.lines,
                    fl.vblank,
                    fl.fps,
                    e.min.as_secs_f64() * 1e3,
                    e.max.as_secs_f64() * 1e3
                );
            }
        }
    }
}

fn check_kernel_data(data: &KernelSensorData) {
    println!(
        "kernel-driver sensor {} (drivers {:?}, verified {}): gain model {:?}, delays {:?}, \
         black level {:?}, extra lines per frame {}, embedded data {}",
        data.name,
        data.drivers,
        data.verified,
        data.analog_gain,
        data.delays
            .map(|d| (d.exposure, d.analog_gain, d.frame_length)),
        data.black_level.map(|b| (b.value, b.bits)),
        data.frame_length_extra_lines,
        data.embedded_data
            .as_ref()
            .map_or("no".into(), |e| format!("{} line(s)", e.lines))
    );
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        for (name, toml) in styx_sensor::BUILTIN_DESCRIPTIONS {
            println!("== built-in description {name}");
            match SensorDescription::from_toml_str(toml, name) {
                Ok(desc) => check_description(&desc),
                Err(e) => println!("{e}"),
            }
        }
        for data in KernelSensorData::builtin() {
            println!("== built-in kernel data {}", data.name);
            check_kernel_data(&data);
        }
        return;
    }
    for path in paths {
        println!("== {path}");
        let checked = if path.ends_with(".kernel.toml") {
            KernelSensorData::from_file(&path).map(|d| check_kernel_data(&d))
        } else {
            SensorDescription::from_file(&path).map(|d| check_description(&d))
        };
        if let Err(e) = checked {
            // Parse errors and every validation problem, each with its field path.
            println!("{e}");
            std::process::exit(1);
        }
    }
}

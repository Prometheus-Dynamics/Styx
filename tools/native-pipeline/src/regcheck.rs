//! `regcheck`: bring the sensor up (power, chip id, init, mode; no streaming) and read back
//! every register the description wrote, to check a way of writing them (one transfer per
//! register, or bursts with the description's `burst_writes`) and a power settle time.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use styx_native::styx_sensor::RegisterBus;
use styx_native::{CameraOptions, NativeCamera, SensorLibrary};

use crate::Args;
use crate::device_run::settings;

pub fn run(a: &Args) -> Result<(), String> {
    let mut lib = SensorLibrary::system();
    if let Some(d) = &a.description {
        lib = lib.with_path_first(d.clone());
    }
    let (cameras, _) = styx_native::discover(&lib);
    let info = cameras
        .into_iter()
        .next()
        .ok_or("no bridged camera found")?;
    let desc = info.description.clone();
    println!(
        "regcheck: {} cycles, burst writes {}, power settle {:?}",
        a.frames, desc.sensor.burst_writes, a.power_settle
    );
    let mut failures = 0;
    for cycle in 0..a.frames {
        let mut options = CameraOptions::default();
        if let Some(s) = a.power_settle {
            options.power_settle = s;
        }
        let t = Instant::now();
        let mut cam = NativeCamera::open(info.clone(), options).map_err(|e| e.to_string())?;
        let configured = cam.configure_external(&settings(a));
        let took = t.elapsed();
        let configured = match configured {
            Ok(c) => c,
            Err(e) => {
                println!("cycle {cycle}: configure failed: {e}");
                failures += 1;
                continue;
            }
        };
        // What each address should hold: the last value written to it.
        let mut want: BTreeMap<u16, u8> = BTreeMap::new();
        let mode = desc
            .mode(&configured.mode.mode)
            .map_err(|e| e.to_string())?;
        let format = desc
            .format_for(mode, &configured.mode.format)
            .map_err(|e| e.to_string())?;
        let lists = [
            &desc.sequences.power_up,
            &desc.sequences.init,
            &format.registers,
            &mode.registers,
        ];
        for w in lists.into_iter().flatten().filter_map(|s| s.as_write()) {
            for i in 0..w.bytes {
                let shift = 8 * u32::from(w.bytes - 1 - i);
                want.insert(w.address + u16::from(i), (w.value >> shift) as u8);
            }
        }
        let controls = cam.controls();
        let mut bad = Vec::new();
        {
            let mut c = controls.shared().lock().map_err(|_| "poisoned")?;
            let bus = c.driver_mut().bus_mut();
            for (&addr, &v) in &want {
                let got = bus.read(addr, 1).map_err(|e| e.to_string())? as u8;
                if got != v {
                    bad.push(format!("{addr:#06x}: wrote {v:#04x}, read {got:#04x}"));
                }
            }
        }
        let b = cam.bring_up_times();
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "cycle {cycle}: configure {:.1} ms (power {:.1}, chip id {:.1}, init {:.1}, mode {:.1}); {} registers, {} differ{}{}",
            ms(took),
            ms(b.power_up),
            ms(b.chip_id),
            ms(b.init),
            ms(b.mode),
            want.len(),
            bad.len(),
            if bad.is_empty() { "" } else { ": " },
            bad.join(", ")
        );
        failures += usize::from(!bad.is_empty());
        cam.close().map_err(|e| e.to_string())?;
    }
    if failures > 0 {
        return Err(format!("{failures} of {} cycles failed", a.frames));
    }
    Ok(())
}

//! Tuning files: Raspberry Pi tuning JSON (the lenient reader and the conversion) and Styx's
//! own TOML tuning, then the algorithm pipeline built from the result, prepared and run over a
//! few simulated frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use styx_algo::sim::{Scene, Simulation};
use styx_algo::tuning::json;
use styx_algo::{CameraConfig, Pipeline, Tuning};

fn run(tuning: &Tuning) {
    // Whatever validated must also round-trip and build a working pipeline.
    if let Ok(text) = tuning.to_toml_string() {
        let _ = Tuning::from_toml_str(&text);
    }
    let Ok(mut pipeline) = Pipeline::from_tuning(tuning) else {
        return;
    };
    let config = CameraConfig::default();
    let Ok(init) = pipeline.prepare(&config).map(Clone::clone) else {
        return;
    };
    let mut sim = Simulation::new(Default::default(), Scene::constant(400.0, 5000.0), &config);
    if let Some(s) = init.sensor {
        sim.start_with(
            s.exposure.as_secs_f64(),
            s.analogue_gain,
            s.frame_duration.as_secs_f64(),
        );
    }
    let _ = sim.run(&mut pipeline, 4);
    let _ = pipeline.warm_state();
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(v) = json::parse(text) {
        let _ = (v.get("version"), v.as_object().map(<[_]>::len));
    }
    if let Ok(import) = Tuning::from_rpi_json_str(text) {
        run(&import.tuning);
    }
    if let Ok(tuning) = Tuning::from_toml_str(text) {
        run(&tuning);
    }
});

//! Record a simulated sequence, then replay it: outputs must match bit for bit.

mod common;

use std::io::BufReader;

use styx_algo::replay::{Recorder, Recording, replay};
use styx_algo::sim::Scene;
use styx_algo::{Controls, Pipeline};

fn record() -> Vec<u8> {
    let config = common::config();
    let mut scene = Scene::constant(30.0, 3200.0);
    scene.lux = Scene::step(30, 30.0, 3000.0);
    scene.ct = Scene::step(50, 3200.0, 6000.0);
    let (mut p, mut sim) = common::start(&common::tuning(), &config, scene);
    let mut rec = Recorder::new(Vec::new(), &config).unwrap();
    let mut frames = sim.run(&mut p, 40);
    // A control change mid-sequence is recorded with the frames.
    sim.controls = Controls {
        ev: 0.5,
        ..Default::default()
    };
    frames.extend(sim.run(&mut p, 40));
    for f in &frames {
        rec.record(&f.stats, &f.meta, Some(&f.params)).unwrap();
    }
    rec.into_inner()
}

#[test]
fn replay_reproduces_outputs_exactly() {
    let bytes = record();
    let recording = Recording::read(BufReader::new(&bytes[..])).unwrap();
    assert_eq!(recording.records.len(), 80);
    assert_eq!(recording.config, common::config());
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    let report = replay(&mut p, &recording).unwrap();
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
    // Replaying twice through the same pipeline is also identical (prepare resets state).
    let again = replay(&mut p, &recording).unwrap();
    assert_eq!(again.outputs, report.outputs);
}

#[test]
fn replay_detects_a_different_tuning() {
    let bytes = record();
    let recording = Recording::read(BufReader::new(&bytes[..])).unwrap();
    let mut tuning = common::tuning();
    tuning.agc.get_or_insert_with(Default::default).speed = 0.5;
    let mut p = Pipeline::from_tuning(&tuning).unwrap();
    let report = replay(&mut p, &recording).unwrap();
    assert!(!report.mismatches.is_empty());
}

#[test]
fn bad_files_report_lines() {
    let e = Recording::read(BufReader::new(
        &b"{\"styx_algo_replay\": 9, \"config\": {}}\n"[..],
    ))
    .unwrap_err()
    .to_string();
    assert!(e.contains("line 1") && e.contains("version"), "{e}");
    let mut bytes = record();
    bytes.extend_from_slice(b"{not json}\n");
    let e = Recording::read(BufReader::new(&bytes[..]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("line 82"), "{e}");
}

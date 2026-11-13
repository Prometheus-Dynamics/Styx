//! Replay recordings made by the reference implementation: the outputs must match bit for bit.
//!
//! `data/replay-sim.jsonl` is 80 frames (a step in light and colour temperature, a control change
//! at frame 40) with the outputs the reference produced when it recorded them. `data/awb-tie-replay.jsonl`
//! is one recorded frame replayed through the reference tie tuning, with its outputs.

use std::io::BufReader;

use styx_aaa::Pipeline;
use styx_aaa::replay::{Recording, replay};

use crate::common;

const SIM: &[u8] = include_bytes!("../data/replay-sim.jsonl");
const TIE: &[u8] = include_bytes!("../data/awb-tie-replay.jsonl");

fn recording(bytes: &[u8]) -> Recording {
    Recording::read(BufReader::new(bytes)).unwrap()
}

/// Every output's serialised form (floats print their shortest round-trip form, so `-0.0` and
/// `0.0` differ) equals the recorded output's.
fn assert_bit_exact(report: &styx_aaa::replay::ReplayReport, rec: &Recording) {
    assert_eq!(report.outputs.len(), rec.records.len());
    for (i, (out, r)) in report.outputs.iter().zip(&rec.records).enumerate() {
        let expected = r.params.as_ref().expect("recorded output");
        let got = serde_json::to_string(out).unwrap();
        let want = serde_json::to_string(expected).unwrap();
        assert_eq!(got, want, "frame {i}");
    }
}

#[test]
fn replay_reproduces_outputs_exactly() {
    let rec = recording(SIM);
    assert_eq!(rec.records.len(), 80);
    assert_eq!(rec.config, common::config());
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    let report = replay(&mut p, &rec).unwrap();
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
    assert_bit_exact(&report, &rec);
    // Replaying twice through the same pipeline is also identical (prepare resets state).
    let again = replay(&mut p, &rec).unwrap();
    assert_eq!(again.outputs, report.outputs);
}

#[test]
fn replay_detects_a_different_tuning() {
    let rec = recording(SIM);
    let mut tuning = common::tuning();
    tuning.agc.get_or_insert_with(Default::default).speed = 0.5;
    let mut p = Pipeline::from_tuning(&tuning).unwrap();
    let report = replay(&mut p, &rec).unwrap();
    assert!(!report.mismatches.is_empty());
}

#[test]
fn awb_tie_replay_matches_the_reference_bit_for_bit() {
    let rec = recording(TIE);
    assert_eq!(rec.records.len(), 1);
    let tuning: styx_aaa::Tuning =
        serde_json::from_str(include_str!("../data/tie-tuning.json")).unwrap();
    let mut p = Pipeline::from_tuning(&tuning).unwrap();
    let report = replay(&mut p, &rec).unwrap();
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
    assert_bit_exact(&report, &rec);
}

#[test]
fn bad_files_report_lines() {
    let e = Recording::read(BufReader::new(
        &b"{\"styx_algo_replay\": 9, \"config\": {}}\n"[..],
    ))
    .unwrap_err()
    .to_string();
    assert!(e.contains("line 1") && e.contains("version"), "{e}");
    let mut bytes = SIM.to_vec();
    bytes.extend_from_slice(b"{not json}\n");
    let e = Recording::read(BufReader::new(&bytes[..]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("line 82"), "{e}");
}

//! Replayed statistics give dev's sensor requests and settings.
//!
//! The input is the recording `styx-aaa` ships (80 frames of a simulated scene, default tuning).
//! The expected output, `data/dev-expected.jsonl`, is dev's `styx_pipeline::Controller` run on the
//! same recording with the same tuning and configuration: its `start` line, then one line per
//! frame with the frame, the sensor request (when news), the lens move and the ISP settings.

use std::fs::File;
use std::io::BufReader;

use serde_json::{Value, json};
use styx_aaa::Tuning;
use styx_aaa::replay::Recording;
use styx_controller::Controller;

const RECORDING: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../aaa/tests/data/replay-sim.jsonl"
);
const DEV_EXPECTED: &str = include_str!("data/dev-expected.jsonl");

#[test]
fn replayed_statistics_give_devs_sensor_requests_and_settings() {
    let file = File::open(RECORDING).expect("the aaa recording");
    let rec = Recording::read(BufReader::new(file)).expect("a recording");
    let mut expected = DEV_EXPECTED
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("a JSON line"));

    let mut c = Controller::new(&Tuning::default(), rec.config.clone()).unwrap();
    let start = c.start().unwrap();
    let want = expected.next().expect("the start line");
    assert_eq!(
        json!({"start": {"sensor": start.sensor, "isp": start.isp}}),
        want
    );

    let (mut requests, mut frames) = (0, 0);
    for r in &rec.records {
        let step = c.process(&r.stats, &r.meta).unwrap();
        let got = json!({
            "frame": step.frame,
            "sensor": step.sensor,
            "lens": step.lens,
            "isp": step.isp,
        });
        let want = expected.next().expect("a line per frame");
        assert_eq!(got, want, "frame {}", step.frame);
        requests += usize::from(step.sensor.is_some());
        // The updates are the request, for the frame it lands on.
        if let (Some(r), Some(u)) = (step.sensor, step.updates) {
            assert!(u.iter().all(|up| up.frame == r.frame));
        }
        frames += 1;
    }
    assert!(
        expected.next().is_none(),
        "dev has more frames than replayed"
    );
    assert_eq!((frames, requests), (80, 11));
}

//! The software ISP loop closed over a virtual sensor: requests land with the sensor's delays,
//! AE converges, grey-world AWB takes out the scene's cast, and the run replays bit for bit
//! from its `styx-algo` recording.
//!
//! With `STYX_RAW_RECORDING=<base>` (a `native-pipeline soft --record` recording) the same loop
//! also runs over the recorded frames.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use styx_algo::replay::{Recording, replay};
use styx_algo::{Pipeline, Tuning};
use styx_pipeline::measure::{rgb_means, settle_index};
use styx_pipeline::rawrec::{FrameRecord, Header, RawRecording, VERSION};
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::{SensorInfo, SensorValues, SoftLoop};
use styx_sensor::SensorDescription;
use styx_softisp::{CfaPattern, OutputBuffers, RawFormat, RawPacking, Scale};

const W: u32 = 256;
const H: u32 = 192;

fn info() -> SensorInfo {
    let desc = SensorDescription::from_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sensor/sensors/ov9782.toml"
    ))
    .unwrap();
    let mut info = SensorInfo::from_description(&desc, "1280x800", "raw10")
        .unwrap()
        .with_fps(30.0, 30.0)
        .unwrap();
    info.width = W;
    info.height = H;
    info
}

/// A BGGR scene under a warm light (R 1.0, G 0.8, B 0.45 of a grey's response) with a
/// brightness ramp, recorded at 10 ms and gain 1 well below saturation.
fn recording() -> RawRecording {
    let info = info();
    let black = 64.0;
    let mut data = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let level = 40.0 + 120.0 * f64::from(x) / f64::from(W);
            let k = match (y & 1, x & 1) {
                (0, 0) => 0.45,
                (1, 1) => 1.0,
                _ => 0.8,
            };
            let v = (black + level * k).round() as u16;
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let sensor = SensorValues {
        frame: 0,
        exposure: Duration::from_millis(10),
        analogue_gain: 1.0,
        digital_gain: 1.0,
        frame_duration: Duration::from_nanos(33_333_300),
        verified: true,
    };
    let header = Header {
        styx_raw_recording: VERSION,
        format: RawFormat::new(W, H, CfaPattern::Bggr, RawPacking::U16Le { bits: 10 }),
        stride: W as usize * 2,
        sensor: info,
        notes: "synthetic".into(),
    };
    let len = data.len() as u64;
    RawRecording::from_parts(
        header,
        vec![FrameRecord {
            sensor,
            timestamp_ns: 0,
            offset: 0,
            len,
        }],
        data,
    )
    .unwrap()
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Run {
    totals: Vec<f64>,
    means: Vec<[f64; 3]>,
    wb: [f64; 3],
    locked: Option<usize>,
}

fn run(rec: &RawRecording, frames: u64, record: Option<Shared>) -> Run {
    let info = rec.header.sensor.clone();
    let mut sensor = VirtualSensor::new(rec);
    let format = sensor.format();
    let mut soft = SoftLoop::new(info, format.packing, &Tuning::default(), 1).unwrap();
    if let Some(r) = record {
        soft.controller().record_to(r).unwrap();
    }
    if let Some(r) = soft.start().unwrap().sensor {
        sensor.request(&r);
    }
    let (w, h) = (format.width as usize, format.height as usize);
    let mut rgb = vec![0u8; w * h * 3];
    let mut out = Run {
        totals: Vec::new(),
        means: Vec::new(),
        wb: [1.0; 3],
        locked: None,
    };
    for i in 0..frames {
        let stride = sensor.stride();
        let (raw, values) = sensor.next_frame();
        let raw = raw.to_vec();
        let o = soft
            .process(
                &raw,
                stride,
                &values,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: w * 3,
                },
            )
            .unwrap();
        if let Some(r) = &o.step.sensor {
            // Requests name a frame the sensor's delays allow.
            assert!(r.frame >= values.frame + 2, "{r:?} at {}", values.frame);
            sensor.request(r);
        }
        out.totals.push(values.total_exposure());
        out.means.push(rgb_means(&rgb, w, h, w * 3));
        out.wb = o.step.isp.wb;
        if o.step.params.ae.locked && out.locked.is_none() {
            out.locked = Some(i as usize);
        }
    }
    soft.controller().stop_recording().unwrap();
    out
}

#[test]
fn the_loop_converges_on_a_virtual_sensor_and_replays() {
    let rec = recording();
    let log = Shared::default();
    let r = run(&rec, 60, Some(log.clone()));
    let settled = settle_index(&r.totals, 0.05).unwrap();
    assert!(
        settled <= 20,
        "exposure settled after {settled} frames: {:?}",
        r.totals
    );
    let locked = r.locked.expect("AE locks");
    assert!(locked <= 25, "locked at {locked}");
    // The light was 4 times too dim at 10 ms: AE asks for more.
    let last = *r.totals.last().unwrap();
    assert!(last > 0.02, "{last}");
    // Grey world: R and B gains take out the warm cast (G/R = 0.8, G/B = 1.78).
    assert!((r.wb[0] - 0.8).abs() < 0.08, "{:?}", r.wb);
    assert!((r.wb[2] - 1.78).abs() < 0.15, "{:?}", r.wb);
    let m = r.means.last().unwrap();
    assert!(
        (m[0] / m[1] - 1.0).abs() < 0.1 && (m[2] / m[1] - 1.0).abs() < 0.1,
        "{m:?}"
    );
    // The algorithms' recording replays exactly.
    let bytes = log.0.lock().unwrap().clone();
    let recording = Recording::read(bytes.as_slice()).unwrap();
    assert_eq!(recording.records.len(), 60);
    let mut p = Pipeline::from_tuning(&Tuning::default()).unwrap();
    assert!(replay(&mut p, &recording).unwrap().mismatches.is_empty());
    // Same inputs, same outputs.
    let again = run(&rec, 60, None);
    assert_eq!(again.totals, r.totals);
}

#[test]
fn recorded_frames_replay_when_given() {
    let Some(base) = std::env::var_os("STYX_RAW_RECORDING") else {
        eprintln!("STYX_RAW_RECORDING not set: skipped");
        return;
    };
    let rec = RawRecording::open(std::path::Path::new(&base)).unwrap();
    let r = run(&rec, rec.len().min(90) as u64, None);
    let settled = settle_index(&r.totals, 0.05).unwrap();
    eprintln!(
        "{} frames: exposure x gain settled after {settled}, locked at {:?}, wb {:?}",
        r.totals.len(),
        r.locked,
        r.wb
    );
    assert!(r.locked.is_some());
}

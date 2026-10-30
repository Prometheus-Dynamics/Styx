//! The synthetic OV9782 scene the loop tests replay.

use std::time::Duration;

use styx_pipeline::rawrec::{FrameRecord, Header, RawRecording, VERSION};
use styx_pipeline::{SensorInfo, SensorValues};
use styx_sensor::SensorDescription;
use styx_softisp::{CfaPattern, RawFormat, RawPacking};

pub const W: u32 = 256;
pub const H: u32 = 192;

pub fn info() -> SensorInfo {
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
pub fn recording() -> RawRecording {
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

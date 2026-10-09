//! The software path's loop on the GPU ISP (feature `gpu`): over a virtual sensor, the loop
//! with `SoftLoop::use_gpu` makes the same pictures, statistics, sensor requests and ISP
//! settings frame by frame as the loop on the software ISP's integer arithmetic. Every Vulkan
//! device of the machine runs it (llvmpipe included); without Vulkan it passes with a note.

use std::time::Duration;

use styx_algo::Tuning;
use styx_pipeline::rawrec::{FrameRecord, Header, RawRecording, VERSION};
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::styx_gpuisp::{DeviceSelect, GpuContext, devices};
use styx_pipeline::{IspEngine, SensorInfo, SensorValues, SoftLoop, soft::base_params};
use styx_sensor::SensorDescription;
use styx_softisp::{
    Arithmetic, CfaPattern, IspParams, OutputBuffers, RawFormat, RawPacking, Scale,
};

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

/// Pictures and loop outputs of `frames` frames, on the CPU or on `gpu`.
fn run(rec: &RawRecording, frames: u64, gpu: Option<&GpuContext>) -> Vec<(Vec<u8>, String)> {
    let mut sensor = VirtualSensor::new(rec);
    let format = sensor.format();
    let mut soft = SoftLoop::new(
        rec.header.sensor.clone(),
        format.packing,
        &Tuning::default(),
        1,
    )
    .unwrap();
    soft.set_base_params(IspParams {
        arithmetic: Arithmetic::Int,
        ..base_params()
    });
    if let Some(ctx) = gpu {
        soft.use_gpu(ctx).unwrap();
        assert!(matches!(soft.engine(), IspEngine::Gpu { .. }));
    }
    if let Some(r) = soft.start().unwrap().sensor {
        sensor.request(&r);
    }
    let (w, h) = (format.width as usize, format.height as usize);
    let mut out = Vec::new();
    for _ in 0..frames {
        let stride = sensor.stride();
        let (raw, values) = sensor.next_frame();
        let raw = raw.to_vec();
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        let (y, uv) = nv12.split_at_mut(w * h);
        let o = soft
            .process(
                &raw,
                stride,
                &values,
                Scale::Full,
                OutputBuffers::Nv12 {
                    y,
                    y_stride: w,
                    uv,
                    uv_stride: w,
                },
            )
            .unwrap();
        if let Some(r) = &o.step.sensor {
            sensor.request(r);
        }
        let step = format!("{:?} {:?} {:?}", o.step.sensor, o.applied, o.stats);
        out.push((nv12, step));
    }
    out
}

#[test]
fn gpu_loop_matches_the_integer_software_isp() {
    let list = devices();
    if list.is_empty() {
        eprintln!("no Vulkan device: skipped");
        return;
    }
    let rec = recording();
    let cpu = run(&rec, 30, None);
    for d in list {
        let ctx = GpuContext::open(DeviceSelect::Index(d.index)).unwrap();
        let gpu = run(&rec, 30, Some(&ctx));
        for (k, (a, b)) in cpu.iter().zip(&gpu).enumerate() {
            assert!(a.0 == b.0, "{}: frame {k} picture", d.name);
            assert_eq!(a.1, b.1, "{}: frame {k} loop", d.name);
        }
    }
}

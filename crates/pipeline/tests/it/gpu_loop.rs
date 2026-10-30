//! The software path's loop on the GPU ISP (feature `gpu`): over a virtual sensor, the loop
//! with `SoftLoop::use_gpu` makes the same pictures, statistics, sensor requests and ISP
//! settings frame by frame as the loop on the software ISP's integer arithmetic. Every Vulkan
//! device of the machine runs it (llvmpipe included); without Vulkan it passes with a note.

use styx_algo::Tuning;
use styx_pipeline::rawrec::RawRecording;
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::styx_gpuisp::{DeviceSelect, GpuContext, devices};
use styx_pipeline::{IspEngine, SoftLoop, soft::base_params};
use styx_softisp::{Arithmetic, IspParams, OutputBuffers, Scale};

use crate::scene::recording;

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

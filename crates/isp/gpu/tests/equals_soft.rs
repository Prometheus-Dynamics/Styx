//! The GPU ISP (`styx-gpu`) equals the software ISP (`styx-soft`) bit for bit on the recorded raw
//! frame: the picture planes and the statistics, and the goldens too. Each test skips with a
//! printed reason when the host has no Vulkan device (the device is probed at run time).
#![cfg(feature = "vulkan")]

#[path = "common/mod.rs"]
mod common;

use common::*;
use styx_aaa::Statistics;
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_gpu::{DeviceSelect, GpuContext, GpuIspStage};
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

fn device() -> Option<GpuContext> {
    if styx_gpu::devices().is_empty() {
        eprintln!("skipped: no suitable Vulkan device on this host");
        return None;
    }
    match GpuContext::open(DeviceSelect::Auto) {
        Ok(ctx) => {
            eprintln!("device: {:?}", ctx.info());
            Some(ctx)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// The picture kinds the goldens cover.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Rgb,
    Nv12,
    I420,
    Luma,
}

impl Kind {
    fn code(self) -> FourCc {
        match self {
            Kind::Rgb => FourCc::RGB3,
            Kind::Nv12 => FourCc::NV12,
            Kind::I420 => FourCc::I420,
            Kind::Luma => FourCc::GREY,
        }
    }

    /// (row bytes, rows) of each plane at `w` x `h`.
    fn planes(self, w: usize, h: usize) -> Vec<(usize, usize)> {
        match self {
            Kind::Rgb => vec![(w * 3, h)],
            Kind::Luma => vec![(w, h)],
            Kind::Nv12 => vec![(w, h), (w, h / 2)],
            Kind::I420 => vec![(w, h), (w / 2, h / 2), (w / 2, h / 2)],
        }
    }
}

struct Case {
    name: &'static str,
    kind: Kind,
    half: bool,
    demosaic: Demosaic,
    yuv: YuvMatrix,
    shaded: bool,
    tone: bool,
    stats: bool,
}

const CASES: [Case; 5] = [
    Case {
        name: "rgb_full",
        kind: Kind::Rgb,
        half: false,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: true,
        stats: true,
    },
    Case {
        name: "nv12_full",
        kind: Kind::Nv12,
        half: false,
        demosaic: Demosaic::Mhc,
        yuv: YuvMatrix::Bt709Limited,
        shaded: false,
        tone: true,
        stats: true,
    },
    Case {
        name: "i420_half",
        kind: Kind::I420,
        half: true,
        demosaic: Demosaic::EdgeDirected,
        yuv: YuvMatrix::Bt601Full,
        shaded: true,
        tone: true,
        stats: true,
    },
    Case {
        name: "luma_full",
        kind: Kind::Luma,
        half: false,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: false,
        stats: true,
    },
    Case {
        name: "rgb_half_nostats",
        kind: Kind::Rgb,
        half: true,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: true,
        stats: false,
    },
];

/// The bytes of a picture's planes, row by row, in plane order (as the golden stores them).
fn packed(lease: &FrameLease<()>, kind: Kind, w: usize, h: usize) -> Vec<u8> {
    let read = lease.read().unwrap();
    let mut out = Vec::new();
    for (plane, (row_bytes, rows)) in kind.planes(w, h).into_iter().enumerate() {
        let plane = read.plane(plane).unwrap();
        for row in 0..rows {
            out.extend_from_slice(plane.row_bytes(row, row_bytes).unwrap());
        }
    }
    out
}

#[test]
fn gpu_equals_soft_and_goldens_bit_for_bit() {
    let Some(ctx) = device() else { return };
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    for case in CASES {
        let (w, h) = if case.half { (W / 2, H / 2) } else { (W, H) };
        let settings = settings(case.shaded, case.tone);
        let options = options(case.demosaic, case.yuv, case.stats);
        let mut cpu = SoftIspStage::new(RAW, W, H, &settings, options.clone()).unwrap();
        let mut gpu = GpuIspStage::with_context(Some(&ctx), RAW, W, H, &settings, options).unwrap();

        let out_pool = picture_pool(case.kind.code(), w, h, 2);
        let mut cpu_out = picture(&out_pool, case.kind.code(), w, h, 2);
        let mut gpu_out = picture(&out_pool, case.kind.code(), w, h, 3);
        let (mut cpu_stats, mut gpu_stats) = (Statistics::default(), Statistics::default());
        cpu.process(&raw, &mut cpu_out, case.stats.then_some(&mut cpu_stats))
            .unwrap_or_else(|e| panic!("soft {}: {e}", case.name));
        gpu.process(&raw, &mut gpu_out, case.stats.then_some(&mut gpu_stats))
            .unwrap_or_else(|e| panic!("gpu {}: {e}", case.name));

        let (cpu_bytes, gpu_bytes) = (
            packed(&cpu_out, case.kind, w as usize, h as usize),
            packed(&gpu_out, case.kind, w as usize, h as usize),
        );
        assert!(
            cpu_bytes == gpu_bytes,
            "{}: GPU picture differs from soft ({} of {} bytes)",
            case.name,
            cpu_bytes
                .iter()
                .zip(&gpu_bytes)
                .filter(|(a, b)| a != b)
                .count(),
            cpu_bytes.len()
        );
        let want = data(&format!("{}.out", case.name));
        assert!(
            gpu_bytes == want,
            "{}: GPU picture differs from the golden",
            case.name
        );
        eprintln!("{}: equal to soft and the golden", case.name);

        if case.stats {
            assert_eq!(
                format!("{:?}", gpu_stats.colour),
                format!("{:?}", cpu_stats.colour),
                "{}: colour statistics",
                case.name
            );
            assert_eq!(
                gpu_stats.histogram.bins(),
                cpu_stats.histogram.bins(),
                "{}: histogram",
                case.name
            );
        }
    }
}

/// The opt-in approximation runs on the GPU too: `ApproxTone`'s quadratics, bit for bit with
/// `styx-soft`'s and its golden (the luma case, where the default sRGB curve's quadratics differ
/// from the table).
#[test]
fn gpu_approx_tone_equals_soft_bit_for_bit() {
    let Some(ctx) = device() else { return };
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    let settings = settings(true, false);
    let mut options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, false);
    options.arithmetic = styx_soft::params::Arithmetic::ApproxTone;
    let mut cpu = SoftIspStage::new(RAW, W, H, &settings, options.clone()).unwrap();
    let mut gpu = GpuIspStage::with_context(Some(&ctx), RAW, W, H, &settings, options).unwrap();
    let out_pool = picture_pool(FourCc::GREY, W, H, 2);
    let mut cpu_out = picture(&out_pool, FourCc::GREY, W, H, 2);
    let mut gpu_out = picture(&out_pool, FourCc::GREY, W, H, 3);
    cpu.process(&raw, &mut cpu_out, None).unwrap();
    gpu.process(&raw, &mut gpu_out, None).unwrap();
    let cpu_bytes = packed(&cpu_out, Kind::Luma, W as usize, H as usize);
    let gpu_bytes = packed(&gpu_out, Kind::Luma, W as usize, H as usize);
    assert!(cpu_bytes == gpu_bytes, "ApproxTone: GPU differs from soft");
    assert!(
        gpu_bytes == data("luma_full.approx.out"),
        "ApproxTone: golden"
    );
    eprintln!("luma_full ApproxTone: equal to soft and the golden");
}

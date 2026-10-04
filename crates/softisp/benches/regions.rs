//! Regions of a 1280x800 RAW10 frame against the whole frame, with the software path's
//! settings (bilinear, lens shading, CCM, sRGB, statistics on every fourth quad row), NV12, one
//! thread: windows, binned overviews (with the whole frame's statistics) and statistics alone.
//!
//! `cargo bench -p styx-softisp --bench regions`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use styx_softisp::*;

const W: usize = 1280;
const H: usize = 800;
const STRIDE: usize = 1600;

/// A textured BGGR RAW10 frame: gradients plus pseudo-random noise.
fn frame() -> Vec<u8> {
    let mut raw = vec![0u8; STRIDE * H];
    let mut seed = 0x1234_5678u32;
    for y in 0..H {
        for g in 0..W / 4 {
            let mut low = 0u8;
            for i in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let x = g * 4 + i;
                let v = ((x + y) as u32 % 700 + 64 + (seed & 63)).min(1023) as u16;
                raw[y * STRIDE + g * 5 + i] = (v >> 2) as u8;
                low |= ((v & 3) as u8) << (2 * i);
            }
            raw[y * STRIDE + g * 5 + 4] = low;
        }
    }
    raw
}

fn params() -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.9,
            g: 1.0,
            b: 1.6,
        }),
        lens_shading: Some(LensShading::radial(16, 12, 0.6)),
        ccm: Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        }),
        tone: Some(ToneCurve::Srgb),
        yuv: YuvMatrix::Bt601Full,
        stats: Some(StatsConfig {
            zones_x: 16,
            zones_y: 12,
            histogram_bins: 256,
            saturation: 0.95,
            row_step: 4,
            ..StatsConfig::default()
        }),
        ..Default::default()
    }
}

/// NV12 buffers of `w`x`h`.
fn nv12(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
    (vec![0; w * h], vec![0; w * h / 2])
}

fn out<'a>(b: &'a mut (Vec<u8>, Vec<u8>), w: usize) -> OutputBuffers<'a> {
    OutputBuffers::Nv12 {
        y: &mut b.0,
        y_stride: w,
        uv: &mut b.1,
        uv_stride: w,
    }
}

fn regions(c: &mut Criterion) {
    let raw = frame();
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let mut isp = SoftIsp::new(format, params())
        .unwrap()
        .with_copy_input(false);
    let mut g = c.benchmark_group("regions_1280x800");
    let mut full = nv12(W, H);
    g.bench_function("full_frame", |bn| {
        bn.iter(|| black_box(isp.process(&raw, STRIDE, Scale::Full, out(&mut full, W))))
    });
    for (w, h) in [(320, 200), (640, 400)] {
        let win = Window::new(480, 300, w as u32, h as u32);
        let mut b = nv12(w, h);
        g.bench_function(format!("window_{w}x{h}"), |bn| {
            bn.iter(|| {
                black_box(isp.process_window(&raw, STRIDE, win, Scale::Full, out(&mut b, w)))
            })
        });
    }
    for factor in [2u32, 4, 8] {
        let (w, h) = isp.binned_size(factor);
        let (w, h) = (w as usize, h as usize);
        let mut b = nv12(w, h);
        g.bench_function(format!("binned_{factor}_with_stats"), |bn| {
            bn.iter(|| black_box(isp.process_binned(&raw, STRIDE, factor, out(&mut b, w))))
        });
    }
    isp.set_statistics(false);
    for factor in [2u32, 4, 8] {
        let (w, h) = isp.binned_size(factor);
        let (w, h) = (w as usize, h as usize);
        let mut b = nv12(w, h);
        g.bench_function(format!("binned_{factor}_alone"), |bn| {
            bn.iter(|| black_box(isp.process_binned(&raw, STRIDE, factor, out(&mut b, w))))
        });
    }
    isp.set_statistics(true);
    g.bench_function("statistics_only", |bn| {
        bn.iter(|| black_box(isp.statistics(&raw, STRIDE)))
    });
    let mut region = nv12(320, 200);
    let mut overview = nv12(320, 200);
    let win = Window::new(480, 300, 320, 200);
    g.bench_function("window_320x200_and_binned_4", |bn| {
        bn.iter(|| {
            let s = isp.process_binned(&raw, STRIDE, 4, out(&mut overview, 320));
            let r = isp.process_window(&raw, STRIDE, win, Scale::Full, out(&mut region, 320));
            black_box((s, r))
        })
    });
    g.finish();
}

fn configure() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

criterion_group! {
    name = benches;
    config = configure();
    targets = regions
}
criterion_main!(benches);

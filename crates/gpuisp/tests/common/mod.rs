//! Synthetic raw frames for the pipeline tests.

#![allow(dead_code)]

use styx_softisp::*;

/// A mosaic of `w` x `h` samples from a per-pixel linear RGB function (sample units).
pub fn mosaic(
    w: usize,
    h: usize,
    pattern: CfaPattern,
    rgb: impl Fn(usize, usize) -> [u16; 3],
) -> Vec<u16> {
    let mut m = vec![0u16; w * h];
    for y in 0..h {
        for x in 0..w {
            let c = rgb(x, y);
            m[y * w + x] = match pattern.channel_at(x, y) {
                Channel::Red => c[0],
                Channel::Green => c[1],
                Channel::Blue => c[2],
            };
        }
    }
    m
}

/// Pack a mosaic; returns the bytes and the stride (padded by 32 bytes, as drivers do).
pub fn pack(m: &[u16], w: usize, h: usize, packing: RawPacking) -> (Vec<u8>, usize) {
    let stride = packing.row_bytes(w) + 32;
    let mut out = vec![0xEEu8; stride * h];
    for y in 0..h {
        let row = &m[y * w..][..w];
        let dst = &mut out[y * stride..];
        match packing {
            RawPacking::U8 => row.iter().enumerate().for_each(|(x, &v)| dst[x] = v as u8),
            RawPacking::U16Le { .. } => row
                .iter()
                .enumerate()
                .for_each(|(x, &v)| dst[2 * x..2 * x + 2].copy_from_slice(&v.to_le_bytes())),
            RawPacking::Csi2Raw10 => {
                for (g, px) in row.chunks(4).enumerate() {
                    let mut low = 0u8;
                    for (i, &v) in px.iter().enumerate() {
                        dst[g * 5 + i] = (v >> 2) as u8;
                        low |= ((v & 3) as u8) << (2 * i);
                    }
                    dst[g * 5 + 4] = low;
                }
            }
            RawPacking::Csi2Raw12 => {
                for (g, px) in row.chunks(2).enumerate() {
                    let mut low = 0u8;
                    for (i, &v) in px.iter().enumerate() {
                        dst[g * 3 + i] = (v >> 4) as u8;
                        low |= ((v & 0xF) as u8) << (4 * i);
                    }
                    dst[g * 3 + 2] = low;
                }
            }
        }
    }
    (out, stride)
}

pub const PATTERNS: [CfaPattern; 4] = [
    CfaPattern::Rggb,
    CfaPattern::Bggr,
    CfaPattern::Grbg,
    CfaPattern::Gbrg,
];

pub const PACKINGS: [RawPacking; 5] = [
    RawPacking::U8,
    RawPacking::U16Le { bits: 10 },
    RawPacking::U16Le { bits: 16 },
    RawPacking::Csi2Raw10,
    RawPacking::Csi2Raw12,
];

/// Every Vulkan device the GPU ISP can use, software rasterisers included (empty, with a
/// note, without Vulkan: the tests then pass vacuously). `STYX_GPUISP_TEST_DEVICE` limits
/// them to one (index or part of the name).
pub fn contexts() -> Vec<styx_gpuisp::GpuContext> {
    use styx_gpuisp::{DeviceSelect, GpuContext};
    let list = styx_gpuisp::devices();
    if list.is_empty() {
        eprintln!("no Vulkan device: GPU ISP tests skipped");
    }
    let only = std::env::var("STYX_GPUISP_TEST_DEVICE").ok();
    list.iter()
        .filter(|d| {
            only.as_ref().is_none_or(|o| {
                o.parse::<usize>().map_or_else(
                    |_| d.name.to_lowercase().contains(&o.to_lowercase()),
                    |i| i == d.index,
                )
            })
        })
        .map(|d| GpuContext::open(DeviceSelect::Index(d.index)).expect("listed device opens"))
        .collect()
}

/// Every output (RGB24, NV12, I420, luma) of one frame at `scale` through `run`, named, and
/// the statistics of the first.
#[allow(clippy::type_complexity)]
pub fn all_outputs(
    (w, h): (usize, usize),
    run: &mut dyn FnMut(OutputBuffers<'_>) -> Option<IspStats>,
) -> (Vec<(&'static str, Vec<u8>)>, Option<IspStats>) {
    let mut rgb = vec![0u8; w * h * 3];
    let stats = run(OutputBuffers::Rgb24 {
        data: &mut rgb,
        stride: w * 3,
    });
    let mut planes = vec![("rgb24", rgb)];
    if w % 2 == 0 && h % 2 == 0 {
        // Padded strides, so that the copy-out honours them.
        let (ys, cs) = (w + 8, w + 4);
        let (mut y, mut uv) = (vec![0u8; ys * h], vec![0u8; cs * h / 2]);
        run(OutputBuffers::Nv12 {
            y: &mut y,
            y_stride: ys,
            uv: &mut uv,
            uv_stride: cs,
        });
        planes.push(("nv12 y", y));
        planes.push(("nv12 uv", uv));
        let (mut y, mut u, mut v) = (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
        run(OutputBuffers::I420 {
            y: &mut y,
            y_stride: w,
            u: &mut u,
            u_stride: w / 2,
            v: &mut v,
            v_stride: w / 2,
        });
        planes.push(("i420 y", y));
        planes.push(("i420 u", u));
        planes.push(("i420 v", v));
    }
    let mut luma = vec![0u8; w * h];
    run(OutputBuffers::Luma {
        data: &mut luma,
        stride: w,
    });
    planes.push(("luma", luma));
    (planes, stats)
}

/// PSNR (dB), largest difference and the share of samples more than 2 codes apart, of
/// interleaved channel `c` of `n`.
pub fn compare(a: &[u8], b: &[u8], c: usize, n: usize) -> (f64, u8, f64) {
    let (mut sq, mut max, mut k, mut over) = (0f64, 0u8, 0usize, 0usize);
    for (p, q) in a.iter().skip(c).step_by(n).zip(b.iter().skip(c).step_by(n)) {
        let d = p.abs_diff(*q);
        sq += f64::from(d) * f64::from(d);
        max = max.max(d);
        over += usize::from(d > 2);
        k += 1;
    }
    let mse = sq / k.max(1) as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    (psnr, max, over as f64 / k.max(1) as f64)
}

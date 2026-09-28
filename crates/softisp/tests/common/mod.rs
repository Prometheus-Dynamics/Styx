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

/// Run the ISP to RGB24 at `scale`.
pub fn rgb(
    format: RawFormat,
    params: &IspParams,
    raw: &[u8],
    stride: usize,
    scale: Scale,
) -> (Vec<u8>, Option<IspStats>) {
    let (w, h) = match scale {
        Scale::Full => (format.width as usize, format.height as usize),
        Scale::Half => (format.width as usize / 2, format.height as usize / 2),
    };
    let mut out = vec![0u8; w * h * 3];
    let stats = process(
        format,
        params,
        raw,
        stride,
        scale,
        OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        },
    )
    .unwrap();
    (out, stats)
}

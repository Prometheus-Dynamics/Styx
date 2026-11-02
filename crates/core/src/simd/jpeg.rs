//! The arithmetic of a baseline JPEG encoder: JFIF colour (RGB to YCbCr 4:2:0) and the forward
//! DCT with quantisation of 8x8 blocks, in exact integer arithmetic. The scalar functions here
//! are the oracle; every vector leaf (RVV) and the GPU's shader (`styx-gpuisp`) give the same
//! bytes and coefficients.
//!
//! Colour (full-range BT.601, as JFIF has it), with `Rs`, `Gs`, `Bs` the sums of a 2x2 block:
//!
//! ```text
//! Y  = (19595 R + 38470 G + 7471 B + 2^15) >> 16
//! Cb = (-11059 Rs - 21709 Gs + 32768 Bs + 128·2^18 + 2^17 - 1) >> 18
//! Cr = (32768 Rs - 27439 Gs - 5329 Bs + 128·2^18 + 2^17 - 1) >> 18
//! ```
//!
//! The chroma rounding is a half less one, so no sum reaches 256: no clamp is needed.
//!
//! DCT: `C[u][x] = round(2^14 · C(u)/2 · cos((2x + 1)uπ/16))` ([`FDCT`]). Rows first, then
//! columns, `>>` rounding half up:
//!
//! ```text
//! t[r][u] = (Σx C[u][x] · (s[r][x] - 128) + 2^10) >> 11
//! F[v][u] = (Σr C[v][r] · t[r][u] + 2^16) >> 17
//! q[v][u] = sign(F) · ((|F| + Q/2) / Q)
//! ```
//!
//! Every intermediate fits an `i32`: the rows reach 46344 · 128 before the shift, the columns
//! 46344 · 2897.

use super::{ColorLayout, SimdBackend};

/// The forward DCT's 8-point basis in Q14: `FDCT[u][x]`.
pub const FDCT: [[i32; 8]; 8] = [
    [5793, 5793, 5793, 5793, 5793, 5793, 5793, 5793],
    [8035, 6811, 4551, 1598, -1598, -4551, -6811, -8035],
    [7568, 3135, -3135, -7568, -7568, -3135, 3135, 7568],
    [6811, -1598, -8035, -4551, 4551, 8035, 1598, -6811],
    [5793, -5793, -5793, 5793, 5793, -5793, -5793, 5793],
    [4551, -8035, 1598, 6811, -6811, -1598, 8035, -4551],
    [3135, -7568, 7568, -3135, -3135, 7568, -7568, 3135],
    [1598, -4551, 6811, -8035, 8035, -6811, 4551, -1598],
];

/// Luma of one pixel.
#[inline]
pub const fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((19595 * r as u32 + 38470 * g as u32 + 7471 * b as u32 + (1 << 15)) >> 16) as u8
}

/// `(Cb, Cr)` of a 2x2 block from its channel sums (each at most 1020).
#[inline]
pub const fn chroma(rs: u32, gs: u32, bs: u32) -> (u8, u8) {
    let (r, g, b) = (rs as i32, gs as i32, bs as i32);
    let bias = (128 << 18) + (1 << 17) - 1;
    let cb = (-11059 * r - 21709 * g + 32768 * b + bias) >> 18;
    let cr = (32768 * r - 27439 * g - 5329 * b + bias) >> 18;
    (cb as u8, cr as u8)
}

/// Two rows of `width` colour pixels (`top`, `bottom`, laid out as `layout`) to two rows of
/// luma and one row of `width.div_ceil(2)` Cb and Cr. An odd last column counts its pixel
/// twice in the 2x2 sum.
#[allow(clippy::too_many_arguments)]
pub fn rgb_ycc420_rows(
    top: &[u8],
    bottom: &[u8],
    y_top: &mut [u8],
    y_bottom: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
    layout: ColorLayout,
) -> SimdBackend {
    let bpp = layout.bytes_per_pixel();
    #[allow(unused_mut)]
    let mut done = 0;
    #[allow(unused_mut)]
    let mut backend = SimdBackend::Scalar;
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    let pairs = width / 2;
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if let Some(b) = super::rvv::jpeg::rgb_ycc420(
        &top[..pairs * 2 * bpp],
        &bottom[..pairs * 2 * bpp],
        &mut y_top[..pairs * 2],
        &mut y_bottom[..pairs * 2],
        &mut cb[..pairs],
        &mut cr[..pairs],
        pairs,
        layout,
    ) {
        (done, backend) = (pairs, b);
    }
    let [ro, go, bo] = layout.rgb_offsets();
    for i in done..width.div_ceil(2) {
        let (mut rs, mut gs, mut bs) = (0, 0, 0);
        for k in 0..2 {
            // The odd last column: its pixel stands for its missing neighbour.
            let x = (2 * i + k).min(width - 1);
            for (row, y) in [(top, &mut *y_top), (bottom, &mut *y_bottom)] {
                let p = &row[x * bpp..x * bpp + bpp];
                y[x] = luma(p[ro], p[go], p[bo]);
                (rs, gs, bs) = (rs + p[ro] as u32, gs + p[go] as u32, bs + p[bo] as u32);
            }
        }
        (cb[i], cr[i]) = chroma(rs, gs, bs);
    }
    backend
}

/// The scalar DCT and quantisation of one block of `samples` rows `stride` apart: `out` in
/// natural (row-major) order. `quant` in natural order too, each entry at least 1.
pub fn fdct_quant_block(samples: &[u8], stride: usize, quant: &[u16; 64], out: &mut [i16]) {
    let mut t = [[0i32; 8]; 8];
    for (r, row) in t.iter_mut().enumerate() {
        let s = &samples[r * stride..r * stride + 8];
        for (u, c) in FDCT.iter().enumerate() {
            let sum: i32 = c.iter().zip(s).map(|(&c, &s)| c * (s as i32 - 128)).sum();
            row[u] = (sum + (1 << 10)) >> 11;
        }
    }
    for (v, c) in FDCT.iter().enumerate() {
        for u in 0..8 {
            let sum: i32 = (0..8).map(|r| c[r] * t[r][u]).sum();
            out[v * 8 + u] = quantize((sum + (1 << 16)) >> 17, quant[v * 8 + u]);
        }
    }
}

/// `sign(f) · ((|f| + q/2) / q)`.
#[inline]
pub const fn quantize(f: i32, q: u16) -> i16 {
    let sign = f >> 31;
    let a = ((f ^ sign) - sign) as u32;
    let q = q as u32;
    let m = ((a + q / 2) / q) as i32;
    ((m ^ sign) - sign) as i16
}

/// DCT and quantisation of `blocks` blocks side by side in an 8-row band (`band`, rows
/// `stride` apart, block `i` at columns `8i..8i + 8`): `out` gets 64 coefficients per block,
/// natural order.
pub fn fdct_quant_band(
    band: &[u8],
    stride: usize,
    blocks: usize,
    quant: &[u16; 64],
    out: &mut [i16],
) -> SimdBackend {
    assert!(band.len() >= 7 * stride + 8 * blocks && out.len() >= 64 * blocks);
    assert!(quant.iter().all(|&q| q > 0), "a quantiser of zero");
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if let Some(b) = super::rvv::jpeg::fdct_quant(band, stride, blocks, quant, out) {
        return b;
    }
    for i in 0..blocks {
        fdct_quant_block(&band[8 * i..], stride, quant, &mut out[64 * i..64 * i + 64]);
    }
    SimdBackend::Scalar
}

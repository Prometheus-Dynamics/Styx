//! Row leaves in RVV 1.0 assembly. Each covers the whole row in strips of the hardware's
//! vector length (`vsetvli`); segment loads and stores split and join the channels.
//!
//! # Safety (every function)
//! The CPU must have the V extension, and the slices must hold `width` pixels of input and
//! output.

use core::arch::asm;

use crate::simd::{ColorLayout, YuvMatrix};

/// One strip: `body` runs with `{left}` pixels left at `{s}` and `{d}`, sets `{n}` to the
/// strip's pixels with its `vsetvli`, and the strip's value is `n`.
macro_rules! strip {
    ($left:expr, $s:expr, $d:expr, [$($body:literal),* $(,)?] $(, $($extra:tt)*)?) => {{
        let n: usize;
        asm!(
            ".option push",
            ".option arch, +v",
            $($body,)*
            ".option pop",
            n = out(reg) n,
            left = in(reg) $left,
            s = inout(reg) $s => _,
            d = inout(reg) $d => _,
            $($($extra)*,)?
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
            out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _,
            out("v17") _, out("v18") _, out("v19") _, out("v20") _, out("v21") _,
            out("v22") _, out("v23") _,
            options(nostack),
        );
        n
    }};
}

/// Strips over `width` pixels: `strip(done, left)` does some of the `left` pixels after the
/// first `done` and returns how many.
fn strips(width: usize, mut strip: impl FnMut(usize, usize) -> usize) {
    let mut done = 0;
    while done < width {
        done += strip(done, width - done);
    }
}

pub(super) unsafe fn swap_rb24(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY (each strip): `n <= left` pixels after `i` remain in both rows (the module).
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(3 * i),
            d.add(3 * i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg3e8.v v0, ({s})",
                "vmv.v.v v8, v4",
                "vmv.v.v v10, v2",
                "vmv.v.v v12, v0",
                "vsseg3e8.v v8, ({d})",
            ]
        )
    });
}

pub(super) unsafe fn swap_rb32(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(4 * i),
            d.add(4 * i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg4e8.v v0, ({s})",
                "vmv.v.v v8, v4",
                "vmv.v.v v10, v2",
                "vmv.v.v v12, v0",
                "vmv.v.v v14, v6",
                "vsseg4e8.v v8, ({d})",
            ]
        )
    });
}

pub(super) unsafe fn x32_to_rgb24(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        if swap {
            strip!(
                left,
                s.add(4 * i),
                d.add(3 * i),
                [
                    "vsetvli {n}, {left}, e8, m2, ta, ma",
                    "vlseg4e8.v v0, ({s})",
                    "vmv.v.v v8, v4",
                    "vmv.v.v v10, v2",
                    "vmv.v.v v12, v0",
                    "vsseg3e8.v v8, ({d})",
                ]
            )
        } else {
            strip!(
                left,
                s.add(4 * i),
                d.add(3 * i),
                [
                    "vsetvli {n}, {left}, e8, m2, ta, ma",
                    "vlseg4e8.v v0, ({s})",
                    "vsseg3e8.v v0, ({d})",
                ]
            )
        }
    });
}

/// `(77 R + 150 G + 29 B) >> 8` with the coefficients put on bytes 0, 1, 2 in the layout's
/// order: widening multiply-adds into 16 bits (at most 65280), narrowed by the shift.
pub(super) unsafe fn rgb_to_luma(src: &[u8], dst: &mut [u8], width: usize, layout: ColorLayout) {
    let mut c = [0usize; 3];
    for (o, k) in layout.rgb_offsets().into_iter().zip([77, 150, 29]) {
        c[o] = k;
    }
    let bpp = layout.bytes_per_pixel();
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        if bpp == 3 {
            strip!(left, s.add(3 * i), d.add(i), [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg3e8.v v0, ({s})",
                "vwmulu.vx v8, v0, {c0}",
                "vwmaccu.vx v8, {c1}, v2",
                "vwmaccu.vx v8, {c2}, v4",
                "vnsrl.wi v12, v8, 8",
                "vse8.v v12, ({d})",
            ], c0 = in(reg) c[0], c1 = in(reg) c[1], c2 = in(reg) c[2])
        } else {
            strip!(left, s.add(4 * i), d.add(i), [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg4e8.v v0, ({s})",
                "vwmulu.vx v8, v0, {c0}",
                "vwmaccu.vx v8, {c1}, v2",
                "vwmaccu.vx v8, {c2}, v4",
                "vnsrl.wi v12, v8, 8",
                "vse8.v v12, ({d})",
            ], c0 = in(reg) c[0], c1 = in(reg) c[1], c2 = in(reg) c[2])
        }
    });
}

pub(super) unsafe fn rgb24_to_rgba(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(3 * i),
            d.add(4 * i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg3e8.v v0, ({s})",
                "vmv.v.i v6, -1",
                "vsseg4e8.v v0, ({d})",
            ]
        )
    });
}

pub(super) unsafe fn gray8_to_rgb24(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(i),
            d.add(3 * i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vle8.v v0, ({s})",
                "vmv.v.v v2, v0",
                "vmv.v.v v4, v0",
                "vsseg3e8.v v0, ({d})",
            ]
        )
    });
}

/// The high bytes, which the two-field segment load puts in v2.
pub(super) unsafe fn gray16le_to_rgb24(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(2 * i),
            d.add(3 * i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg2e8.v v0, ({s})",
                "vmv.v.v v0, v2",
                "vmv.v.v v4, v2",
                "vsseg3e8.v v0, ({d})",
            ]
        )
    });
}

/// 16-bit channels narrowed to their high bytes (e16 at LMUL 2 and e8 at LMUL 1 have the same
/// vector length, so one `vl` serves both).
pub(super) unsafe fn rgb48le_to_rgb24(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        if swap {
            strip!(
                left,
                s.add(6 * i),
                d.add(3 * i),
                [
                    "vsetvli {n}, {left}, e16, m2, ta, ma",
                    "vlseg3e16.v v0, ({s})",
                    "vsetvli zero, {n}, e8, m1, ta, ma",
                    "vnsrl.wi v8, v4, 8",
                    "vnsrl.wi v9, v2, 8",
                    "vnsrl.wi v10, v0, 8",
                    "vsseg3e8.v v8, ({d})",
                ]
            )
        } else {
            strip!(
                left,
                s.add(6 * i),
                d.add(3 * i),
                [
                    "vsetvli {n}, {left}, e16, m2, ta, ma",
                    "vlseg3e16.v v0, ({s})",
                    "vsetvli zero, {n}, e8, m1, ta, ma",
                    "vnsrl.wi v8, v0, 8",
                    "vnsrl.wi v9, v2, 8",
                    "vnsrl.wi v10, v4, 8",
                    "vsseg3e8.v v8, ({d})",
                ]
            )
        }
    });
}

pub(super) unsafe fn yuyv_luma(src: &[u8], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        strip!(
            left,
            s.add(2 * i),
            d.add(i),
            [
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg2e8.v v0, ({s})",
                "vse8.v v0, ({d})",
            ]
        )
    });
}

/// YUYV chroma of two rows: segment loads split even and odd bytes; `vaaddu` with
/// round-to-nearest-up (`vxrm` 0) is (x + y + 1) >> 1.
pub(super) unsafe fn yuyv_chroma(a: &[u8], b: &[u8], dst: &mut [u8], width: usize) {
    let (x, y, d) = (a.as_ptr(), b.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above, `2n` bytes of each source row.
    strips(width, |i, left| unsafe {
        strip!(left, x.add(2 * i), d.add(i), [
            "vsetvli {n}, {left}, e8, m2, ta, ma",
            "csrwi vxrm, 0",
            "vlseg2e8.v v0, ({s})",
            "vlseg2e8.v v4, ({b})",
            "vaaddu.vv v8, v2, v6",
            "vse8.v v8, ({d})",
        ], b = in(reg) y.add(2 * i))
    });
}

/// 2x2 box average: segment loads split even and odd bytes; the sums widen to 16 bits (at
/// most 1020 + 2) and narrow back with a shift by 2, as the scalar kernel rounds.
pub(super) unsafe fn box2(top: &[u8], bottom: &[u8], dst: &mut [u8], width: usize) {
    let (t, b, d) = (top.as_ptr(), bottom.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above, `2n` bytes of each source row.
    strips(width, |i, left| unsafe {
        strip!(left, t.add(2 * i), d.add(i), [
            "vsetvli {n}, {left}, e8, m2, ta, ma",
            "vlseg2e8.v v0, ({s})",
            "vlseg2e8.v v4, ({b})",
            "vwaddu.vv v8, v0, v2",
            "vwaddu.vv v12, v4, v6",
            "vsetvli zero, {n}, e16, m4, ta, ma",
            "vadd.vv v8, v8, v12",
            "vadd.vi v8, v8, 2",
            "vsetvli zero, {n}, e8, m2, ta, ma",
            "vnsrl.wi v16, v8, 2",
            "vse8.v v16, ({d})",
        ], b = in(reg) b.add(2 * i))
    });
}

/// Pixel order reversed: each strip of source pixels `i..i + n` gathered backwards (16-bit
/// indices `n - 1 - k`, so any vector length works) into output pixels `width - i - n..`.
pub(super) unsafe fn reverse(src: &[u8], dst: &mut [u8], width: usize, bpp: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above; the strip's output pixels end where the previous strip's start.
    strips(width, |i, left| unsafe {
        let (src, out) = (s.add(i * bpp), d.add((width - i) * bpp));
        match bpp {
            1 => strip!(left, src, out, [
                "vsetvli {n}, {left}, e8, m1, ta, ma",
                "sub {d}, {d}, {n}",
                "vle8.v v0, ({s})",
                "addi {t}, {n}, -1",
                "vsetvli zero, {n}, e16, m2, ta, ma",
                "vid.v v8",
                "vrsub.vx v8, v8, {t}",
                "vsetvli zero, {n}, e8, m1, ta, ma",
                "vrgatherei16.vv v4, v0, v8",
                "vse8.v v4, ({d})",
            ], t = out(reg) _),
            2 => strip!(left, src, out, [
                "vsetvli {n}, {left}, e16, m1, ta, ma",
                "slli {t}, {n}, 1",
                "sub {d}, {d}, {t}",
                "vle16.v v0, ({s})",
                "addi {t}, {n}, -1",
                "vid.v v8",
                "vrsub.vx v8, v8, {t}",
                "vrgather.vv v4, v0, v8",
                "vse16.v v4, ({d})",
            ], t = out(reg) _),
            3 => strip!(left, src, out, [
                "vsetvli {n}, {left}, e8, m1, ta, ma",
                "slli {t}, {n}, 1",
                "add {t}, {t}, {n}",
                "sub {d}, {d}, {t}",
                "vlseg3e8.v v0, ({s})",
                "addi {t}, {n}, -1",
                "vsetvli zero, {n}, e16, m2, ta, ma",
                "vid.v v8",
                "vrsub.vx v8, v8, {t}",
                "vsetvli zero, {n}, e8, m1, ta, ma",
                "vrgatherei16.vv v4, v0, v8",
                "vrgatherei16.vv v5, v1, v8",
                "vrgatherei16.vv v6, v2, v8",
                "vsseg3e8.v v4, ({d})",
            ], t = out(reg) _),
            _ => strip!(left, src, out, [
                "vsetvli {n}, {left}, e32, m1, ta, ma",
                "slli {t}, {n}, 2",
                "sub {d}, {d}, {t}",
                "vle32.v v0, ({s})",
                "addi {t}, {n}, -1",
                "vid.v v8",
                "vrsub.vx v8, v8, {t}",
                "vrgather.vv v4, v0, v8",
                "vse32.v v4, ({d})",
            ], t = out(reg) _),
        }
    });
}

/// YUV → RGB over `pairs` pixel pairs a strip, from the even lumas in v0, U in v1, the odd
/// lumas in v2 and V in v3 (each strip's `$load`, at e8 m1). Chroma terms and luma widen to
/// 32 bits (m4) as the scalar kernel computes; negatives go to 0, then two `vnclipu` (rounding
/// down: `vxrm` 2) shift by 8 and clamp to 255. The six results, red green blue of the even
/// pixel then of the odd, are one 6-field segment store.
macro_rules! yuv_rgb_strip {
    ($left:expr, $s:expr, $d:expr, $m:expr, [$($load:literal),* $(,)?] $(, $($extra:tt)*)?) => {{
        let m: &YuvMatrix = $m;
        strip!($left, $s, $d, [
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            $($load,)*
            "csrwi vxrm, 2",
            "vssubu.vx v0, v0, {off}",
            "vssubu.vx v2, v2, {off}",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vzext.vf4 v8, v1",
            "vsub.vx v8, v8, {h}",
            "vzext.vf4 v12, v3",
            "vsub.vx v12, v12, {h}",
            "vzext.vf4 v16, v0",
            "vmul.vx v16, v16, {sc}",
            "vadd.vx v16, v16, {h}",
            "vzext.vf4 v20, v2",
            "vmul.vx v20, v20, {sc}",
            "vadd.vx v20, v20, {h}",
            // Red: + r_v·e.
            "vmul.vx v24, v12, {rv}",
            "vadd.vv v28, v16, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v0, v6, 0",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vadd.vv v28, v20, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v3, v6, 0",
            // Green: - g_u·d - g_v·e.
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vmul.vx v24, v8, {gu}",
            "vmacc.vx v24, {gv}, v12",
            "vsub.vv v28, v16, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v1, v6, 0",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vsub.vv v28, v20, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v4, v6, 0",
            // Blue: + b_u·d.
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vmul.vx v24, v8, {bu}",
            "vadd.vv v28, v16, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v2, v6, 0",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vadd.vv v28, v20, v24",
            "vmax.vx v28, v28, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vnclipu.wi v6, v28, 8",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnclipu.wi v5, v6, 0",
            "vsseg6e8.v v0, ({d})",
        ],
            off = in(reg) usize::from(m.y_offset),
            sc = in(reg) usize::from(m.y_scale),
            h = in(reg) 128usize,
            rv = in(reg) usize::from(m.r_v),
            gu = in(reg) usize::from(m.g_u),
            gv = in(reg) usize::from(m.g_v),
            bu = in(reg) usize::from(m.b_u),
            out("v24") _, out("v25") _, out("v26") _, out("v27") _,
            out("v28") _, out("v29") _, out("v30") _, out("v31") _
            $(, $($extra)*)?)
    }};
}

/// NV12 → RGB24 over pixel pairs.
pub(super) unsafe fn nv12_rgb(y: &[u8], uv: &[u8], dst: &mut [u8], pairs: usize, m: &YuvMatrix) {
    let (s, c, d) = (y.as_ptr(), uv.as_ptr(), dst.as_mut_ptr());
    // SAFETY (each strip): `n <= left` pairs after `i` remain in every row (the module).
    strips(pairs, |i, left| unsafe {
        yuv_rgb_strip!(left, s.add(2 * i), d.add(6 * i), m, [
            "vlseg2e8.v v4, ({c})",
            "vlseg2e8.v v6, ({s})",
            "vmv1r.v v0, v6",
            "vmv1r.v v1, v4",
            "vmv1r.v v2, v7",
            "vmv1r.v v3, v5",
        ], c = in(reg) c.add(2 * i))
    });
}

/// I420 → RGB24 over pixel pairs.
pub(super) unsafe fn i420_rgb(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    dst: &mut [u8],
    pairs: usize,
    m: &YuvMatrix,
) {
    let (s, cu, cv, d) = (y.as_ptr(), u.as_ptr(), v.as_ptr(), dst.as_mut_ptr());
    // SAFETY (each strip): `n <= left` pairs after `i` remain in every row (the module).
    strips(pairs, |i, left| unsafe {
        yuv_rgb_strip!(left, s.add(2 * i), d.add(6 * i), m, [
            "vle8.v v1, ({u})",
            "vle8.v v3, ({v})",
            "vlseg2e8.v v4, ({s})",
            "vmv1r.v v0, v4",
            "vmv1r.v v2, v5",
        ], u = in(reg) cu.add(i), v = in(reg) cv.add(i))
    });
}

/// YUYV → RGB24 over pixel pairs: a 4-field segment load splits Y0 U Y1 V.
pub(super) unsafe fn yuyv_rgb(src: &[u8], dst: &mut [u8], pairs: usize, m: &YuvMatrix) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY (each strip): `n <= left` pairs after `i` remain in both rows (the module).
    strips(pairs, |i, left| unsafe {
        yuv_rgb_strip!(
            left,
            s.add(4 * i),
            d.add(6 * i),
            m,
            ["vlseg4e8.v v0, ({s})"]
        )
    });
}

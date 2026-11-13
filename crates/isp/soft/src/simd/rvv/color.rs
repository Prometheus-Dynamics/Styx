//! Colour-side leaves in RVV 1.0 assembly: the colour matrix, narrowing, the tone curve
//! (table and quadratics), RGB interleaving and the YCbCr conversions.
//!
//! The fixed-point multiplies use `vsmul` with `vxrm` set to round-to-nearest-up, which is
//! the scalar kernels' `(a b + 2^14) >> 15` (`pmulhrsw`, `sqrdmulh`) for 16-bit lanes.
//!
//! # Safety (every function)
//! The CPU must have the V extension, and the slices must hold what the scalar kernel reads
//! and writes for `width` pixels.

use super::{rvv, strips};
use crate::simd::YuvCoeffs;

/// Colour matrix in place: inputs times 8, nine `vsmul` terms with saturating sums, clamped to
/// 0..=4095; all three outputs are computed before any is stored.
pub(super) unsafe fn ccm(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize) {
    let [r, g, b] = planes.map(<[u16]>::as_mut_ptr);
    let m = m.as_ptr();
    // SAFETY (each strip): `n <= left` pixels after `i` remain in every plane.
    strips(width, |i, left| unsafe {
        rvv!([
            "csrwi vxrm, 0",
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({r})",
            "vle16.v v4, ({g})",
            "vle16.v v6, ({b})",
            "vsll.vi v2, v2, 3",
            "vsll.vi v4, v4, 3",
            "vsll.vi v6, v6, 3",
            "lh {k}, 0({m})",
            "vsmul.vx v8, v2, {k}",
            "lh {k}, 2({m})",
            "vsmul.vx v14, v4, {k}",
            "vsadd.vv v8, v8, v14",
            "lh {k}, 4({m})",
            "vsmul.vx v14, v6, {k}",
            "vsadd.vv v8, v8, v14",
            "lh {k}, 6({m})",
            "vsmul.vx v10, v2, {k}",
            "lh {k}, 8({m})",
            "vsmul.vx v14, v4, {k}",
            "vsadd.vv v10, v10, v14",
            "lh {k}, 10({m})",
            "vsmul.vx v14, v6, {k}",
            "vsadd.vv v10, v10, v14",
            "lh {k}, 12({m})",
            "vsmul.vx v12, v2, {k}",
            "lh {k}, 14({m})",
            "vsmul.vx v14, v4, {k}",
            "vsadd.vv v12, v12, v14",
            "lh {k}, 16({m})",
            "vsmul.vx v14, v6, {k}",
            "vsadd.vv v12, v12, v14",
            "li {k}, 4095",
            "vmax.vx v8, v8, zero",
            "vmax.vx v10, v10, zero",
            "vmax.vx v12, v12, zero",
            "vmin.vx v8, v8, {k}",
            "vmin.vx v10, v10, {k}",
            "vmin.vx v12, v12, {k}",
            "vse16.v v8, ({r})",
            "vse16.v v10, ({g})",
            "vse16.v v12, ({b})",
        ], left = in(reg) left, r = in(reg) r.add(i), g = in(reg) g.add(i), b = in(reg) b.add(i),
           m = in(reg) m, k = out(reg) _)
    });
}

/// `min(s >> 4, 255)` narrowed to bytes.
pub(super) unsafe fn narrow(src: &[u16], dst: &mut [u8], width: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({s})",
            "vsrl.vi v2, v2, 4",
            "vminu.vx v2, v2, {max}",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v4, v2, 0",
            "vse8.v v4, ({d})",
        ], left = in(reg) left, s = in(reg) s.add(i), d = in(reg) d.add(i), max = in(reg) 255usize)
    });
}

/// The 4096-entry table by an indexed load: inputs clamped to 4095 are the byte offsets
/// (16-bit indices for byte data, so e8 at LMUL 1 pairs with e16 at LMUL 2).
pub(super) unsafe fn lut(src: &[u16], dst: &mut [u8], lut: &[u8; 4096], width: usize) {
    let (s, d, t) = (src.as_ptr(), dst.as_mut_ptr(), lut.as_ptr());
    // SAFETY: as above; every index is at most 4095.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({s})",
            "vminu.vx v2, v2, {max}",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vluxei16.v v4, ({t}), v2",
            "vse8.v v4, ({d})",
        ], left = in(reg) left, s = in(reg) s.add(i), d = in(reg) d.add(i), t = in(reg) t,
           max = in(reg) 4095usize)
    });
}

/// The quadratic tone curve ([`crate::simd::poly`]): `v = min(x, 4095) + 32`, its octave from
/// six compares, `t = (v << (9 - octave)) - 2^14`, the segment index `8 half + octave` gathered
/// from the three 16-entry coefficient tables, then Horner's rule with `vsmul` / `vsadd`.
pub(super) unsafe fn poly(src: &[u16], dst: &mut [u8], c: &[[[i16; 8]; 2]; 3], width: usize) {
    let (s, d, c) = (src.as_ptr(), dst.as_mut_ptr(), c.as_ptr().cast::<i16>());
    // SAFETY: as above; the tables are 16 entries each, the indices at most 15.
    strips(width, |i, left| unsafe {
        rvv!([
            "csrwi vxrm, 0",
            "vsetivli zero, 16, e16, m2, ta, ma",
            "vle16.v v10, ({c})",
            "addi {t}, {c}, 32",
            "vle16.v v12, ({t})",
            "addi {t}, {c}, 64",
            "vle16.v v14, ({t})",
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({s})",
            "li {t}, 4095",
            "vminu.vx v2, v2, {t}",
            "li {t}, 32",
            "vadd.vx v2, v2, {t}",
            "vmv.v.i v4, 0",
            "li {t}, 64",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            "li {t}, 128",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            "li {t}, 256",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            "li {t}, 512",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            "li {t}, 1024",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            "li {t}, 2048",
            "vmsgeu.vx v0, v2, {t}",
            "vadd.vi v4, v4, 1, v0.t",
            // t = (v << (9 - octave)) - 2^14
            "vrsub.vi v6, v4, 9",
            "vsll.vv v6, v2, v6",
            "li {t}, 16384",
            "vsub.vx v6, v6, {t}",
            // index = 8 min(t >> 13, 1) + octave
            "vsrl.vi v8, v6, 13",
            "li {t}, 1",
            "vminu.vx v8, v8, {t}",
            "vsll.vi v8, v8, 3",
            "vadd.vv v8, v8, v4",
            "vrgather.vv v16, v10, v8",
            "vrgather.vv v18, v12, v8",
            "vrgather.vv v20, v14, v8",
            "vsmul.vv v22, v20, v6",
            "vsadd.vv v22, v18, v22",
            "vsmul.vv v22, v22, v6",
            "vsadd.vv v22, v16, v22",
            "li {t}, 1024",
            "vsmul.vx v22, v22, {t}",
            "vmax.vx v22, v22, zero",
            "li {t}, 255",
            "vmin.vx v22, v22, {t}",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v24, v22, 0",
            "vse8.v v24, ({d})",
        ], left = in(reg) left, s = in(reg) s.add(i), d = in(reg) d.add(i), c = in(reg) c,
           t = out(reg) _)
    });
}

/// Planar R, G, B to packed RGB24 by a three-field segment store.
pub(super) unsafe fn interleave_rgb(planes: [&[u8]; 3], dst: &mut [u8], width: usize) {
    let [r, g, b] = planes.map(<[u8]>::as_ptr);
    let d = dst.as_mut_ptr();
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m2, ta, ma",
            "vle8.v v2, ({r})",
            "vle8.v v4, ({g})",
            "vle8.v v6, ({b})",
            "vsseg3e8.v v2, ({d})",
        ], left = in(reg) left, r = in(reg) r.add(i), g = in(reg) g.add(i), b = in(reg) b.add(i),
           d = in(reg) d.add(3 * i))
    });
}

/// Luma: widening multiply-adds (at most 65280 + 128), `>> 8`, offset, `min 255`, narrowed.
pub(super) unsafe fn rgb_to_y(planes: [&[u8]; 3], dst: &mut [u8], width: usize, c: &YuvCoeffs) {
    let [r, g, b] = planes.map(<[u8]>::as_ptr);
    let d = dst.as_mut_ptr();
    let [c0, c1, c2] = c.y.map(usize::from);
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vle8.v v1, ({r})",
            "vle8.v v2, ({g})",
            "vle8.v v3, ({b})",
            "vwmulu.vx v4, v1, {c0}",
            "vwmaccu.vx v4, {c1}, v2",
            "vwmaccu.vx v4, {c2}, v3",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "li {t}, 128",
            "vadd.vx v4, v4, {t}",
            "vsrl.vi v4, v4, 8",
            "vadd.vx v4, v4, {off}",
            "li {t}, 255",
            "vminu.vx v4, v4, {t}",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v6, v4, 0",
            "vse8.v v6, ({d})",
        ], left = in(reg) left, r = in(reg) r.add(i), g = in(reg) g.add(i), b = in(reg) b.add(i),
           d = in(reg) d.add(i), c0 = in(reg) c0, c1 = in(reg) c1, c2 = in(reg) c2,
           off = in(reg) usize::from(c.y_offset), t = out(reg) _)
    });
}

/// 4:2:0 chroma: each channel's 2x2 means (segment loads of both rows, widening sums), then
/// `128 + ((k R + k G + k B + 64) >> 7)` in signed 16 bits (at most 32640 in magnitude),
/// clamped, narrowed and stored interleaved (NV12) or to two planes.
// Many borrowed rows, as the scalar oracle.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn rgb_to_uv(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) {
    let [tr, tg, tb] = top.map(<[u8]>::as_ptr);
    let [br, bg, bb] = bottom.map(<[u8]>::as_ptr);
    let (up, vp) = (u.as_mut_ptr(), v.as_mut_ptr());
    let k: [i16; 6] = [c.u[0], c.u[1], c.u[2], c.v[0], c.v[1], c.v[2]];
    let kp = k.as_ptr();
    // SAFETY: as above; `2n` samples of each row, `n` (or `2n` interleaved) chroma bytes.
    strips(width, |i, left| unsafe {
        macro_rules! uv {
            ($($store:literal),*) => {
                rvv!([
                    "vsetvli {n}, {left}, e8, m1, ta, ma",
                    "vlseg2e8.v v1, ({tr})",
                    "vlseg2e8.v v3, ({br})",
                    "vlseg2e8.v v5, ({tg})",
                    "vlseg2e8.v v7, ({bg})",
                    "vlseg2e8.v v9, ({tb})",
                    "vlseg2e8.v v11, ({bb})",
                    "vwaddu.vv v14, v1, v2",
                    "vwaddu.vv v20, v3, v4",
                    "vwaddu.vv v16, v5, v6",
                    "vwaddu.vv v22, v7, v8",
                    "vwaddu.vv v18, v9, v10",
                    "vwaddu.vv v24, v11, v12",
                    "vsetvli zero, {n}, e16, m2, ta, ma",
                    "vadd.vv v14, v14, v20",
                    "vadd.vv v16, v16, v22",
                    "vadd.vv v18, v18, v24",
                    "vadd.vi v14, v14, 2",
                    "vadd.vi v16, v16, 2",
                    "vadd.vi v18, v18, 2",
                    "vsrl.vi v14, v14, 2",
                    "vsrl.vi v16, v16, 2",
                    "vsrl.vi v18, v18, 2",
                    "lh {t}, 0({k})",
                    "vmul.vx v20, v14, {t}",
                    "lh {t}, 2({k})",
                    "vmacc.vx v20, {t}, v16",
                    "lh {t}, 4({k})",
                    "vmacc.vx v20, {t}, v18",
                    "lh {t}, 6({k})",
                    "vmul.vx v22, v14, {t}",
                    "lh {t}, 8({k})",
                    "vmacc.vx v22, {t}, v16",
                    "lh {t}, 10({k})",
                    "vmacc.vx v22, {t}, v18",
                    "li {t}, 64",
                    "vadd.vx v20, v20, {t}",
                    "vadd.vx v22, v22, {t}",
                    "vsra.vi v20, v20, 7",
                    "vsra.vi v22, v22, 7",
                    "li {t}, 128",
                    "vadd.vx v20, v20, {t}",
                    "vadd.vx v22, v22, {t}",
                    "vmax.vx v20, v20, zero",
                    "vmax.vx v22, v22, zero",
                    "li {t}, 255",
                    "vmin.vx v20, v20, {t}",
                    "vmin.vx v22, v22, {t}",
                    "vsetvli zero, {n}, e8, m1, ta, ma",
                    "vnsrl.wi v26, v20, 0",
                    "vnsrl.wi v27, v22, 0",
                    $($store,)*
                ], left = in(reg) left, tr = in(reg) tr.add(2 * i), tg = in(reg) tg.add(2 * i),
                   tb = in(reg) tb.add(2 * i), br = in(reg) br.add(2 * i),
                   bg = in(reg) bg.add(2 * i), bb = in(reg) bb.add(2 * i),
                   u = in(reg) up.add(if interleaved { 2 * i } else { i }),
                   v = in(reg) vp.add(if interleaved { 0 } else { i }),
                   k = in(reg) kp, t = out(reg) _)
            };
        }
        if interleaved {
            uv!("vsseg2e8.v v26, ({u})", "/* {v} */")
        } else {
            uv!("vse8.v v26, ({u})", "vse8.v v27, ({v})")
        }
    });
}

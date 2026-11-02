//! Mosaic-side leaves in RVV 1.0 assembly: unpacking, the front end, the demosaics, luma from
//! the mosaic, the half-size quads and the statistics. Each covers whole rows in strips of
//! the hardware's vector length; colour sites alternate, so the demosaics compute both sites'
//! formulas for every lane and pick per lane with a parity mask (`v0`).
//!
//! # Safety (every function)
//! The CPU must have the V extension, and the slices must hold what the scalar kernel reads
//! and writes for `width` pixels.

use super::{rvv, strips};
use crate::format::CfaPattern;
use crate::simd::RowKind;

/// RAW10: `groups` groups of four pixels in five bytes. The five-field segment load splits
/// the high bytes and the low-bits byte; a four-field store interleaves the pixels.
pub(super) unsafe fn unpack_raw10(src: &[u8], dst: &mut [u16], groups: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY (each strip): `n <= left` groups after `i` remain in both rows.
    strips(groups, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vlseg5e8.v v1, ({s})",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vzext.vf2 v16, v5",
            "vzext.vf2 v8, v1",
            "vsll.vi v8, v8, 2",
            "vand.vi v18, v16, 3",
            "vor.vv v8, v8, v18",
            "vzext.vf2 v10, v2",
            "vsll.vi v10, v10, 2",
            "vsrl.vi v18, v16, 2",
            "vand.vi v18, v18, 3",
            "vor.vv v10, v10, v18",
            "vzext.vf2 v12, v3",
            "vsll.vi v12, v12, 2",
            "vsrl.vi v18, v16, 4",
            "vand.vi v18, v18, 3",
            "vor.vv v12, v12, v18",
            "vzext.vf2 v14, v4",
            "vsll.vi v14, v14, 2",
            "vsrl.vi v18, v16, 6",
            "vor.vv v14, v14, v18",
            "vsseg4e16.v v8, ({d})",
        ], left = in(reg) left, s = in(reg) s.add(5 * i), d = in(reg) d.add(4 * i))
    });
}

/// RAW12: `pairs` pairs of pixels in three bytes.
pub(super) unsafe fn unpack_raw12(src: &[u8], dst: &mut [u16], pairs: usize) {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(pairs, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vlseg3e8.v v1, ({s})",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vzext.vf2 v16, v3",
            "vzext.vf2 v8, v1",
            "vsll.vi v8, v8, 4",
            "vand.vi v18, v16, 15",
            "vor.vv v8, v8, v18",
            "vzext.vf2 v10, v2",
            "vsll.vi v10, v10, 4",
            "vsrl.vi v18, v16, 4",
            "vor.vv v10, v10, v18",
            "vsseg2e16.v v8, ({d})",
        ], left = in(reg) left, s = in(reg) s.add(3 * i), d = in(reg) d.add(2 * i))
    });
}

/// Front end on `pairs` column pairs: even and odd columns by a two-field segment load, each
/// with its black level (saturating subtract), shifted, scaled by the high half of the
/// product with its gain and clamped to 4095.
pub(super) unsafe fn front(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    pairs: usize,
) {
    let (r, g) = (row.as_mut_ptr(), gains.as_ptr());
    // SAFETY: as above.
    strips(pairs, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vlseg2e16.v v2, ({r})",
            "vlseg2e16.v v6, ({g})",
            "vssubu.vx v2, v2, {b0}",
            "vssubu.vx v4, v4, {b1}",
            "vsll.vx v2, v2, {sh}",
            "vsll.vx v4, v4, {sh}",
            "vmulhu.vv v2, v2, v6",
            "vmulhu.vv v4, v4, v8",
            "vminu.vx v2, v2, {max}",
            "vminu.vx v4, v4, {max}",
            "vsseg2e16.v v2, ({r})",
        ], left = in(reg) left, r = in(reg) r.add(2 * i), g = in(reg) g.add(2 * i),
           b0 = in(reg) black[0] as usize, b1 = in(reg) black[1] as usize,
           sh = in(reg) shift as usize, max = in(reg) 4095usize)
    });
}

/// The parity mask of a strip starting at column `i`: lanes whose column is green.
/// (Inlined into each demosaic as `vid`, add, and, compare into `v0`.)
fn green_parity(kind: RowKind) -> usize {
    usize::from(!kind.green_even)
}

/// Bilinear demosaic: both sites' values for every lane (`h`, `v` and `d` fit 16 bits for
/// 12-bit inputs), merged by the green mask; `x` and `y` are the row's own colour and the
/// other, stored to red or blue by the row kind.
pub(super) unsafe fn demosaic_bilinear(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) {
    let [up, cur, dn] = rows.map(<[u16]>::as_ptr);
    let [r, g, b] = out.map(<[u16]>::as_mut_ptr);
    let (xo, yo) = if kind.x_is_red { (r, b) } else { (b, r) };
    let par = green_parity(kind);
    // SAFETY: as above; the rows hold `width + 2` samples from column -1.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({c})",
            "addi {t}, {c}, 2",
            "vle16.v v4, ({t})",
            "addi {t}, {c}, 4",
            "vle16.v v6, ({t})",
            "vle16.v v8, ({u})",
            "addi {t}, {u}, 2",
            "vle16.v v10, ({t})",
            "addi {t}, {u}, 4",
            "vle16.v v12, ({t})",
            "vle16.v v14, ({w})",
            "addi {t}, {w}, 2",
            "vle16.v v16, ({t})",
            "addi {t}, {w}, 4",
            "vle16.v v18, ({t})",
            // h, v, d
            "vadd.vv v20, v2, v6",
            "vadd.vv v22, v10, v16",
            "vadd.vv v24, v8, v12",
            "vadd.vv v24, v24, v14",
            "vadd.vv v24, v24, v18",
            // green site: x = (h + 1) >> 1, y = (v + 1) >> 1
            "vadd.vi v26, v20, 1",
            "vsrl.vi v26, v26, 1",
            "vadd.vi v28, v22, 1",
            "vsrl.vi v28, v28, 1",
            // other site: g = (h + v + 2) >> 2, y = (d + 2) >> 2
            "vadd.vv v30, v20, v22",
            "vadd.vi v30, v30, 2",
            "vsrl.vi v30, v30, 2",
            "vadd.vi v8, v24, 2",
            "vsrl.vi v8, v8, 2",
            // lanes on green columns
            "vid.v v2",
            "vadd.vx v2, v2, {i}",
            "vand.vi v2, v2, 1",
            "vmseq.vx v0, v2, {par}",
            "vmerge.vvm v10, v30, v4, v0",
            "vmerge.vvm v12, v4, v26, v0",
            "vmerge.vvm v14, v8, v28, v0",
            "vse16.v v10, ({go})",
            "vse16.v v12, ({xo})",
            "vse16.v v14, ({yo})",
        ], left = in(reg) left, c = in(reg) cur.add(i), u = in(reg) up.add(i), w = in(reg) dn.add(i),
           go = in(reg) g.add(i), xo = in(reg) xo.add(i), yo = in(reg) yo.add(i),
           i = in(reg) i, par = in(reg) par, t = out(reg) _)
    });
}

/// Malvar-He-Cutler: the neighbourhood sums in 16 bits, the kernels in 32 (e16 at LMUL 1
/// widens to e32 at LMUL 2), `(s + 8) >> 4` clamped, narrowed and merged by the green mask.
pub(super) unsafe fn demosaic_mhc(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) {
    let [u2, u1, c0, d1, d2] = rows.map(<[u16]>::as_ptr);
    let [r, g, b] = out.map(<[u16]>::as_mut_ptr);
    let (xo, yo) = if kind.x_is_red { (r, b) } else { (b, r) };
    let par = green_parity(kind);
    // SAFETY: as above; the rows hold `width + 4` samples from column -2.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            // c, far_h, near_h from the centre row (columns x-2 .. x+2 at offsets 0..8 bytes)
            "addi {t}, {c}, 4",
            "vle16.v v1, ({t})",
            "vle16.v v2, ({c})",
            "addi {t}, {c}, 8",
            "vle16.v v7, ({t})",
            "vadd.vv v2, v2, v7",
            "addi {t}, {c}, 2",
            "vle16.v v4, ({t})",
            "addi {t}, {c}, 6",
            "vle16.v v7, ({t})",
            "vadd.vv v4, v4, v7",
            // far_v, near_v, diag
            "addi {t}, {u2}, 4",
            "vle16.v v3, ({t})",
            "addi {t}, {d2}, 4",
            "vle16.v v7, ({t})",
            "vadd.vv v3, v3, v7",
            "addi {t}, {u1}, 4",
            "vle16.v v5, ({t})",
            "addi {t}, {d1}, 4",
            "vle16.v v7, ({t})",
            "vadd.vv v5, v5, v7",
            "addi {t}, {u1}, 2",
            "vle16.v v6, ({t})",
            "addi {t}, {u1}, 6",
            "vle16.v v7, ({t})",
            "vadd.vv v6, v6, v7",
            "addi {t}, {d1}, 2",
            "vle16.v v7, ({t})",
            "vadd.vv v6, v6, v7",
            "addi {t}, {d1}, 6",
            "vle16.v v7, ({t})",
            "vadd.vv v6, v6, v7",
            // widen: C v8, FH v10, FV v12, NH v14, NV v16, DG v18
            "vsetvli zero, {n}, e32, m2, ta, ma",
            "vzext.vf2 v8, v1",
            "vzext.vf2 v10, v2",
            "vzext.vf2 v12, v3",
            "vzext.vf2 v14, v4",
            "vzext.vf2 v16, v5",
            "vzext.vf2 v18, v6",
            // green X: 10c + 8nh - 2(fh + dg) + fv
            "vmul.vx v22, v8, {ten}",
            "vsll.vi v20, v14, 3",
            "vadd.vv v22, v22, v20",
            "vadd.vv v20, v10, v18",
            "vsll.vi v20, v20, 1",
            "vsub.vv v22, v22, v20",
            "vadd.vv v22, v22, v12",
            // green Y: 10c + 8nv - 2(fv + dg) + fh
            "vmul.vx v24, v8, {ten}",
            "vsll.vi v20, v16, 3",
            "vadd.vv v24, v24, v20",
            "vadd.vv v20, v12, v18",
            "vsll.vi v20, v20, 1",
            "vsub.vv v24, v24, v20",
            "vadd.vv v24, v24, v10",
            // other G: 8c + 4(nh + nv) - 2(fh + fv); other Y: 12c + 4dg - 3(fh + fv)
            "vadd.vv v20, v10, v12",
            "vsll.vi v26, v8, 3",
            "vadd.vv v28, v14, v16",
            "vsll.vi v28, v28, 2",
            "vadd.vv v26, v26, v28",
            "vsll.vi v28, v20, 1",
            "vsub.vv v26, v26, v28",
            "vmul.vx v28, v8, {twelve}",
            "vsll.vi v30, v18, 2",
            "vadd.vv v28, v28, v30",
            "vsll.vi v30, v20, 1",
            "vadd.vv v30, v30, v20",
            "vsub.vv v28, v28, v30",
            // (s + 8) >> 4 clamped to 0..=4095
            "vadd.vi v22, v22, 8",
            "vadd.vi v24, v24, 8",
            "vadd.vi v26, v26, 8",
            "vadd.vi v28, v28, 8",
            "vsra.vi v22, v22, 4",
            "vsra.vi v24, v24, 4",
            "vsra.vi v26, v26, 4",
            "vsra.vi v28, v28, 4",
            "vmax.vx v22, v22, zero",
            "vmax.vx v24, v24, zero",
            "vmax.vx v26, v26, zero",
            "vmax.vx v28, v28, zero",
            "vmin.vx v22, v22, {max}",
            "vmin.vx v24, v24, {max}",
            "vmin.vx v26, v26, {max}",
            "vmin.vx v28, v28, {max}",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vnsrl.wi v2, v22, 0",
            "vnsrl.wi v3, v24, 0",
            "vnsrl.wi v4, v26, 0",
            "vnsrl.wi v5, v28, 0",
            "vid.v v6",
            "vadd.vx v6, v6, {i}",
            "vand.vi v6, v6, 1",
            "vmseq.vx v0, v6, {par}",
            "vmerge.vvm v6, v4, v1, v0",
            "vmerge.vvm v7, v1, v2, v0",
            "vmerge.vvm v9, v5, v3, v0",
            "vse16.v v6, ({go})",
            "vse16.v v7, ({xo})",
            "vse16.v v9, ({yo})",
        ], left = in(reg) left, c = in(reg) c0.add(i), u2 = in(reg) u2.add(i),
           u1 = in(reg) u1.add(i), d1 = in(reg) d1.add(i), d2 = in(reg) d2.add(i),
           go = in(reg) g.add(i), xo = in(reg) xo.add(i), yo = in(reg) yo.add(i),
           i = in(reg) i, par = in(reg) par, ten = in(reg) 10usize, twelve = in(reg) 12usize,
           max = in(reg) 4095usize, t = out(reg) _)
    });
}

/// See [`crate::simd::scalar::demosaic_edge_row`], in 16-bit lanes as the x86 kernel carries
/// them: the opposite colour plus 24576 fits unsigned 16 bits; `vaadd`/`vaaddu` rounding down
/// (`vxrm` 2) are the floor means; `vssubu` of 6144 from its quarter is the clamp at 0. Ties
/// keep the mean, a smaller gradient's estimate replaces it.
pub(super) unsafe fn demosaic_edge(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) {
    let [u2, u1, c0, d1, d2] = rows.map(<[u16]>::as_ptr);
    let [r, g, b] = out.map(<[u16]>::as_mut_ptr);
    let (xo, yo) = if kind.x_is_red { (r, b) } else { (b, r) };
    let par = green_parity(kind);
    // SAFETY: as above; the rows hold `width + 4` samples from column -2.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            "csrwi vxrm, 2",
            // c (v1); w2, w1, e1, e2 (v2..v5); n2, n1, s1, s2 (v6..v9); nw1, ne1, sw1, se1
            // (v10..v13).
            "addi {t}, {c}, 4",
            "vle16.v v1, ({t})",
            "vle16.v v2, ({c})",
            "addi {t}, {c}, 2",
            "vle16.v v3, ({t})",
            "addi {t}, {c}, 6",
            "vle16.v v4, ({t})",
            "addi {t}, {c}, 8",
            "vle16.v v5, ({t})",
            "addi {t}, {u2}, 4",
            "vle16.v v6, ({t})",
            "addi {t}, {u1}, 4",
            "vle16.v v7, ({t})",
            "addi {t}, {d1}, 4",
            "vle16.v v8, ({t})",
            "addi {t}, {d2}, 4",
            "vle16.v v9, ({t})",
            "addi {t}, {u1}, 2",
            "vle16.v v10, ({t})",
            "addi {t}, {u1}, 6",
            "vle16.v v11, ({t})",
            "addi {t}, {d1}, 2",
            "vle16.v v12, ({t})",
            "addi {t}, {d1}, 6",
            "vle16.v v13, ({t})",
            // dh (v14), dv (v15): sums of absolute differences.
            "vmaxu.vv v14, v2, v1",
            "vminu.vv v15, v2, v1",
            "vsub.vv v14, v14, v15",
            "vmaxu.vv v15, v5, v1",
            "vminu.vv v16, v5, v1",
            "vsub.vv v15, v15, v16",
            "vadd.vv v14, v14, v15",
            "vmaxu.vv v15, v3, v4",
            "vminu.vv v16, v3, v4",
            "vsub.vv v15, v15, v16",
            "vadd.vv v14, v14, v15",
            "vmaxu.vv v15, v6, v1",
            "vminu.vv v16, v6, v1",
            "vsub.vv v15, v15, v16",
            "vmaxu.vv v16, v9, v1",
            "vminu.vv v17, v9, v1",
            "vsub.vv v16, v16, v17",
            "vadd.vv v15, v15, v16",
            "vmaxu.vv v16, v7, v8",
            "vminu.vv v17, v7, v8",
            "vsub.vv v16, v16, v17",
            "vadd.vv v15, v15, v16",
            // gh4 (v16), gv4 (v17), g4 (v18).
            "vadd.vv v16, v3, v4",
            "vadd.vv v16, v16, v1",
            "vsll.vi v16, v16, 1",
            "vsub.vv v16, v16, v2",
            "vsub.vv v16, v16, v5",
            "vadd.vv v17, v7, v8",
            "vadd.vv v17, v17, v1",
            "vsll.vi v17, v17, 1",
            "vsub.vv v17, v17, v6",
            "vsub.vv v17, v17, v9",
            "vaadd.vv v18, v16, v17",
            "vmsltu.vv v0, v15, v14",
            "vmerge.vvm v18, v18, v17, v0",
            "vmsltu.vv v0, v14, v15",
            "vmerge.vvm v18, v18, v16, v0",
            // base = g4 + 24576 - (w1 + e1 + n1 + s1) (v19).
            "vadd.vv v19, v3, v4",
            "vadd.vv v20, v7, v8",
            "vadd.vv v19, v19, v20",
            "vsub.vv v19, v18, v19",
            "vadd.vx v19, v19, {off}",
            // d_1 (v20), d_2 (v21).
            "vadd.vv v21, v3, v7",
            "vadd.vv v22, v4, v8",
            "vmaxu.vv v20, v21, v22",
            "vminu.vv v23, v21, v22",
            "vsub.vv v20, v20, v23",
            "vmaxu.vv v21, v10, v13",
            "vminu.vv v22, v10, v13",
            "vsub.vv v21, v21, v22",
            "vsll.vi v21, v21, 1",
            "vadd.vv v20, v20, v21",
            "vadd.vv v22, v4, v7",
            "vadd.vv v23, v3, v8",
            "vmaxu.vv v21, v22, v23",
            "vminu.vv v24, v22, v23",
            "vsub.vv v21, v21, v24",
            "vmaxu.vv v22, v11, v12",
            "vminu.vv v23, v11, v12",
            "vsub.vv v22, v22, v23",
            "vsll.vi v22, v22, 1",
            "vadd.vv v21, v21, v22",
            // y1 (v22), y2 (v23), y4 (v24), its quarter less the offset.
            "vadd.vv v22, v10, v13",
            "vsll.vi v22, v22, 1",
            "vadd.vv v22, v22, v19",
            "vadd.vv v23, v11, v12",
            "vsll.vi v23, v23, 1",
            "vadd.vv v23, v23, v19",
            "vaaddu.vv v24, v22, v23",
            "vmsltu.vv v0, v21, v20",
            "vmerge.vvm v24, v24, v23, v0",
            "vmsltu.vv v0, v20, v21",
            "vmerge.vvm v24, v24, v22, v0",
            "vadd.vi v24, v24, 2",
            "vsrl.vi v24, v24, 2",
            "vssubu.vx v24, v24, {qoff}",
            "vminu.vx v24, v24, {max}",
            // Quarters of gh4, gv4 and g4, clamped to 0..=4095.
            "vadd.vi v16, v16, 2",
            "vadd.vi v17, v17, 2",
            "vadd.vi v18, v18, 2",
            "vsra.vi v16, v16, 2",
            "vsra.vi v17, v17, 2",
            "vsra.vi v18, v18, 2",
            "vmax.vx v16, v16, zero",
            "vmax.vx v17, v17, zero",
            "vmax.vx v18, v18, zero",
            "vmin.vx v16, v16, {max}",
            "vmin.vx v17, v17, {max}",
            "vmin.vx v18, v18, {max}",
            // Green sites (v0): green c, X from gh4, Y from gv4; elsewhere g4, c, y4.
            "vid.v v25",
            "vadd.vx v25, v25, {i}",
            "vand.vi v25, v25, 1",
            "vmseq.vx v0, v25, {par}",
            "vmerge.vvm v26, v18, v1, v0",
            "vmerge.vvm v27, v1, v16, v0",
            "vmerge.vvm v28, v24, v17, v0",
            "vse16.v v26, ({go})",
            "vse16.v v27, ({xo})",
            "vse16.v v28, ({yo})",
        ], left = in(reg) left, c = in(reg) c0.add(i), u2 = in(reg) u2.add(i),
           u1 = in(reg) u1.add(i), d1 = in(reg) d1.add(i), d2 = in(reg) d2.add(i),
           go = in(reg) g.add(i), xo = in(reg) xo.add(i), yo = in(reg) yo.add(i),
           i = in(reg) i, par = in(reg) par, off = in(reg) 24576usize,
           qoff = in(reg) 6144usize, max = in(reg) 4095usize, t = out(reg) _)
    });
}

/// Luma from the mosaic: column sums `up + 2 cur + dn` (at most 16380) and the row filter
/// `a + 2 b + c` (at most 65520) fit 16 bits.
pub(super) unsafe fn bayer_luma(rows: [&[u16]; 3], dst: &mut [u16], width: usize) {
    let [up, cur, dn] = rows.map(<[u16]>::as_ptr);
    let d = dst.as_mut_ptr();
    // SAFETY: as above; the rows hold `width + 2` samples.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({u})",
            "vle16.v v4, ({c})",
            "vle16.v v6, ({w})",
            "vsll.vi v4, v4, 1",
            "vadd.vv v2, v2, v4",
            "vadd.vv v2, v2, v6",
            "addi {t}, {u}, 2",
            "vle16.v v8, ({t})",
            "addi {t}, {c}, 2",
            "vle16.v v10, ({t})",
            "addi {t}, {w}, 2",
            "vle16.v v12, ({t})",
            "vsll.vi v10, v10, 1",
            "vadd.vv v8, v8, v10",
            "vadd.vv v8, v8, v12",
            "addi {t}, {u}, 4",
            "vle16.v v14, ({t})",
            "addi {t}, {c}, 4",
            "vle16.v v16, ({t})",
            "addi {t}, {w}, 4",
            "vle16.v v18, ({t})",
            "vsll.vi v16, v16, 1",
            "vadd.vv v14, v14, v16",
            "vadd.vv v14, v14, v18",
            "vsll.vi v8, v8, 1",
            "vadd.vv v2, v2, v8",
            "vadd.vv v2, v2, v14",
            "vadd.vi v2, v2, 8",
            "vsrl.vi v2, v2, 4",
            "vse16.v v2, ({d})",
        ], left = in(reg) left, u = in(reg) up.add(i), c = in(reg) cur.add(i), w = in(reg) dn.add(i),
           d = in(reg) d.add(i), t = out(reg) _)
    });
}

/// One RGB pixel per quad: top and bottom pairs by segment loads (t0 v1, t1 v2, b0 v3, b1 v4),
/// the green mean of the pattern's diagonal in v5, each output stored from its register.
pub(super) unsafe fn quad_rgb(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) {
    let (t, b) = (top.as_ptr(), bottom.as_ptr());
    let [ro, go, bo] = out.map(<[u16]>::as_mut_ptr);
    macro_rules! quad {
        ($ga:literal, $gb:literal, $r:literal, $b:literal) => {
            // SAFETY: as above; `2n` samples of each row.
            strips(width, |i, left| unsafe {
                rvv!([
                    "vsetvli {n}, {left}, e16, m1, ta, ma",
                    "vlseg2e16.v v1, ({t})",
                    "vlseg2e16.v v3, ({b})",
                    concat!("vadd.vv v5, ", $ga, ", ", $gb),
                    "vadd.vi v5, v5, 1",
                    "vsrl.vi v5, v5, 1",
                    concat!("vse16.v ", $r, ", ({ro})"),
                    "vse16.v v5, ({go})",
                    concat!("vse16.v ", $b, ", ({bo})"),
                ], left = in(reg) left, t = in(reg) t.add(2 * i), b = in(reg) b.add(2 * i),
                   ro = in(reg) ro.add(i), go = in(reg) go.add(i), bo = in(reg) bo.add(i))
            })
        };
    }
    match pattern {
        CfaPattern::Rggb => quad!("v2", "v3", "v1", "v4"),
        CfaPattern::Bggr => quad!("v2", "v3", "v4", "v1"),
        CfaPattern::Grbg => quad!("v1", "v4", "v2", "v3"),
        CfaPattern::Gbrg => quad!("v1", "v4", "v3", "v2"),
    }
}

/// Quad luma: `(t0 + t1 + b0 + b1 + 2) >> 2`.
pub(super) unsafe fn quad_luma(top: &[u16], bottom: &[u16], dst: &mut [u16], width: usize) {
    let (t, b, d) = (top.as_ptr(), bottom.as_ptr(), dst.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vlseg2e16.v v2, ({t})",
            "vlseg2e16.v v6, ({b})",
            "vadd.vv v2, v2, v4",
            "vadd.vv v2, v2, v6",
            "vadd.vv v2, v2, v8",
            "vadd.vi v2, v2, 2",
            "vsrl.vi v2, v2, 2",
            "vse16.v v2, ({d})",
        ], left = in(reg) left, t = in(reg) t.add(2 * i), b = in(reg) b.add(2 * i), d = in(reg) d.add(i))
    });
}

/// Zone sums per strip: the clip mask (`max(R, G, B) < sat`) in `v0`, masked widening sum
/// reductions for R, G, B, `vcpop` for the count, an unmasked one for the luma.
pub(super) unsafe fn zone_sums(rgb: [&[u16]; 3], width: usize, sat: u16) -> [u32; 5] {
    let [r, g, b] = rgb.map(<[u16]>::as_ptr);
    let mut sums = [0u32; 5];
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        let (sr, sg, sb, sc, sy): (usize, usize, usize, usize, usize);
        let n = rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({r})",
            "vle16.v v4, ({g})",
            "vle16.v v6, ({b})",
            "vmaxu.vv v8, v2, v4",
            "vmaxu.vv v8, v8, v6",
            "vmsltu.vx v0, v8, {sat}",
            "vcpop.m {sc}, v0",
            "vsll.vi v10, v4, 1",
            "vadd.vv v10, v10, v2",
            "vadd.vv v10, v10, v6",
            "vadd.vi v10, v10, 2",
            "vsrl.vi v10, v10, 2",
            "vsetivli zero, 1, e32, m1, ta, ma",
            "vmv.s.x v12, zero",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vwredsumu.vs v13, v2, v12, v0.t",
            "vwredsumu.vs v14, v4, v12, v0.t",
            "vwredsumu.vs v15, v6, v12, v0.t",
            "vwredsumu.vs v16, v10, v12",
            "vsetivli zero, 1, e32, m1, ta, ma",
            "vmv.x.s {sr}, v13",
            "vmv.x.s {sg}, v14",
            "vmv.x.s {sb}, v15",
            "vmv.x.s {sy}, v16",
        ], left = in(reg) left, r = in(reg) r.add(i), g = in(reg) g.add(i), b = in(reg) b.add(i),
           sat = in(reg) sat as usize, sr = out(reg) sr, sg = out(reg) sg, sb = out(reg) sb,
           sc = out(reg) sc, sy = out(reg) sy);
        // `vmv.x.s` sign-extends the 32-bit element; the sums are unsigned.
        for (s, v) in sums.iter_mut().zip([sr, sg, sb, sc, sy]) {
            *s = s.wrapping_add(v as u32);
        }
        n
    });
    sums
}

/// Histogram bins: quad luma in 16 bits, times `bins` widened to 32, `>> 12` narrowed back.
pub(super) unsafe fn luma_bins(rgb: [&[u16]; 3], dst: &mut [u16], bins: u16, width: usize) {
    let [r, g, b] = rgb.map(<[u16]>::as_ptr);
    let d = dst.as_mut_ptr();
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e16, m2, ta, ma",
            "vle16.v v2, ({r})",
            "vle16.v v4, ({g})",
            "vle16.v v6, ({b})",
            "vsll.vi v4, v4, 1",
            "vadd.vv v2, v2, v4",
            "vadd.vv v2, v2, v6",
            "vadd.vi v2, v2, 2",
            "vsrl.vi v2, v2, 2",
            "vwmulu.vx v8, v2, {bins}",
            "vnsrl.wi v12, v8, 12",
            "vse16.v v12, ({d})",
        ], left = in(reg) left, r = in(reg) r.add(i), g = in(reg) g.add(i), b = in(reg) b.add(i),
           d = in(reg) d.add(i), bins = in(reg) bins as usize)
    });
}

//! The output picture's stages in RISC-V vector assembly. The samples are widened to e16 at LMUL
//! 2; the products that leave 16 bits (a Q8 scale of a difference) go to e32 at LMUL 4 and back.
//!
//! # Safety (every function)
//! The CPU must have the V extension, and the slices must hold `width` samples of input and
//! output; the filters' rows hold two more samples than `dst`, read from the column before it.

use super::{SimdBackend, run, strips};
use crate::isp::post::RCP;

/// One sigma-filter neighbour at `{p}`: `v4` centres, `v8` thresholds, `v10` sum, `v12` count.
macro_rules! neighbour {
    ($base:literal, $at:literal) => {
        concat!(
            "addi {p}, {",
            $base,
            "}, ",
            $at,
            "\n",
            "vsetvli zero, {n}, e8, m1, ta, ma\n",
            "vle8.v v1, ({p})\n",
            "vsetvli zero, {n}, e16, m2, ta, mu\n",
            "vzext.vf2 v6, v1\n",
            "vsub.vv v14, v6, v4\n",
            "vrsub.vi v16, v14, 0\n",
            "vmax.vv v14, v14, v16\n",
            "vsll.vi v14, v14, 2\n",
            "vmsleu.vv v0, v14, v8\n",
            "vadd.vv v10, v10, v6, v0.t\n",
            "vadd.vi v12, v12, 1, v0.t\n",
        )
    };
}

/// `v` as e16, clamped to bytes and stored at `{}` (`v1` scratch).
macro_rules! store_clamped {
    ($v:literal, $to:literal) => {
        concat!(
            "vmax.vx ",
            $v,
            ", ",
            $v,
            ", zero\n",
            "li {t}, 255\n",
            "vminu.vx ",
            $v,
            ", ",
            $v,
            ", {t}\n",
            "vsetvli zero, {n}, e8, m1, ta, ma\n",
            "vnsrl.wi v1, ",
            $v,
            ", 0\n",
            "vse8.v v1, ({",
            $to,
            "})\n",
            "vsetvli zero, {n}, e16, m2, ta, ma\n",
        )
    };
}

/// `v12 = (v6 {k} + 128) >> 8`, through e32.
macro_rules! scale_q8 {
    ($k:literal) => {
        concat!(
            "vwmul.vx v24, v6, {",
            $k,
            "}\n",
            "vsetvli zero, {n}, e32, m4, ta, ma\n",
            "li {t}, 128\n",
            "vadd.vx v24, v24, {t}\n",
            "vsetvli zero, {n}, e16, m2, ta, mu\n",
            "vnsra.wi v12, v24, 8\n",
        )
    };
}

unsafe fn rgb_to_planes(rgb: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8], width: usize) {
    let (s, py, pb, pr) = (
        rgb.as_ptr(),
        y.as_mut_ptr(),
        cb.as_mut_ptr(),
        cr.as_mut_ptr(),
    );
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        // The sums reach 32640 and only the rounding can pass i16: saturating it gives
        // 127 + 128 = 255, which the clamp would; the lowest result is 1, so no other clamp.
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vlseg3e8.v v1, ({s})",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vzext.vf2 v4, v1",
            "vzext.vf2 v6, v2",
            "vzext.vf2 v8, v3",
            "li {t}, 77",
            "vmul.vx v10, v4, {t}",
            "li {t}, 150",
            "vmacc.vx v10, {t}, v6",
            "li {t}, 29",
            "vmacc.vx v10, {t}, v8",
            "vadd.vx v10, v10, {k}",
            "vsrl.vi v10, v10, 8",
            "vsll.vi v12, v8, 7",
            "li {t}, 43",
            "vnmsac.vx v12, {t}, v4",
            "li {t}, 85",
            "vnmsac.vx v12, {t}, v6",
            "vsadd.vx v12, v12, {k}",
            "vsra.vi v12, v12, 8",
            "vadd.vx v12, v12, {k}",
            "vsll.vi v14, v4, 7",
            "li {t}, 107",
            "vnmsac.vx v14, {t}, v6",
            "li {t}, 21",
            "vnmsac.vx v14, {t}, v8",
            "vsadd.vx v14, v14, {k}",
            "vsra.vi v14, v14, 8",
            "vadd.vx v14, v14, {k}",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v1, v10, 0",
            "vse8.v v1, ({y})",
            "vnsrl.wi v1, v12, 0",
            "vse8.v v1, ({cb})",
            "vnsrl.wi v1, v14, 0",
            "vse8.v v1, ({cr})",
        ], left = in(reg) left, s = in(reg) s.add(3 * i), y = in(reg) py.add(i),
           cb = in(reg) pb.add(i), cr = in(reg) pr.add(i), k = in(reg) 128usize,
           t = out(reg) _)
    });
}

unsafe fn planes_to_rgb(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize) {
    let [py, pb, pr] = planes.map(<[u8]>::as_ptr);
    let [r, g, b] = rgb.map(|p| p.as_mut_ptr());
    // SAFETY: as above.
    strips(width, |i, left| unsafe {
        // `359 = 256 + 103`, `-183 = -256 + 73`, `454 = 512 - 58` keep the products in 16 bits.
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vle8.v v1, ({y})",
            "vle8.v v2, ({cb})",
            "vle8.v v3, ({cr})",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vzext.vf2 v4, v1",
            "vzext.vf2 v6, v2",
            "vzext.vf2 v8, v3",
            "vsub.vx v6, v6, {k}",
            "vsub.vx v8, v8, {k}",
            "li {t}, 103",
            "vmul.vx v10, v8, {t}",
            "vadd.vx v10, v10, {k}",
            "vsra.vi v10, v10, 8",
            "vadd.vv v10, v10, v8",
            "vadd.vv v10, v10, v4",
            store_clamped!("v10", "r"),
            "li {t}, 73",
            "vmul.vx v10, v8, {t}",
            "li {t}, 88",
            "vnmsac.vx v10, {t}, v6",
            "vadd.vx v10, v10, {k}",
            "vsra.vi v10, v10, 8",
            "vsub.vv v10, v10, v8",
            "vadd.vv v10, v10, v4",
            store_clamped!("v10", "g"),
            "li {t}, 58",
            "vmul.vx v10, v6, {t}",
            "vrsub.vx v10, v10, {k}",
            "vsra.vi v10, v10, 8",
            "vadd.vv v10, v10, v6",
            "vadd.vv v10, v10, v6",
            "vadd.vv v10, v10, v4",
            store_clamped!("v10", "b"),
        ], left = in(reg) left, y = in(reg) py.add(i), cb = in(reg) pb.add(i),
           cr = in(reg) pr.add(i), r = in(reg) r.add(i), g = in(reg) g.add(i),
           b = in(reg) b.add(i), k = in(reg) 128usize, t = out(reg) _)
    });
}

unsafe fn sigma(rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16) {
    let [up, cur, dn] = rows.map(<[u8]>::as_ptr);
    let out = dst.as_mut_ptr();
    // SAFETY: as above.
    strips(dst.len(), |i, left| unsafe {
        // The result lies between the centre and the mean: no clamp before narrowing.
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "addi {p}, {c}, 1",
            "vle8.v v1, ({p})",
            "li {t}, 10",
            "vsetvli zero, {t}, e16, m2, ta, ma",
            "vle16.v v18, ({rcp})",
            "vsetvli zero, {n}, e16, m2, ta, mu",
            "vzext.vf2 v4, v1",
            "vadd.vv v6, v4, v4",
            "vluxei16.v v8, ({table}), v6",
            "vmv.v.v v10, v4",
            "vmv.v.i v12, 1",
            neighbour!("u", "0"),
            neighbour!("u", "1"),
            neighbour!("u", "2"),
            neighbour!("c", "0"),
            neighbour!("c", "2"),
            neighbour!("d", "0"),
            neighbour!("d", "1"),
            neighbour!("d", "2"),
            "vrgather.vv v14, v18, v12",
            "vmulhu.vv v16, v10, v14",
            "vmul.vv v6, v10, v14",
            "vsrl.vi v6, v6, 15",
            "vadd.vv v16, v16, v6",
            "vsub.vv v6, v16, v4",
            scale_q8!("s"),
            "vadd.vv v4, v4, v12",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v1, v4, 0",
            "vse8.v v1, ({out})",
        ], left = in(reg) left, u = in(reg) up.add(i), c = in(reg) cur.add(i),
           d = in(reg) dn.add(i), out = in(reg) out.add(i), table = in(reg) table.as_ptr(),
           rcp = in(reg) RCP.as_ptr(), s = in(reg) isize::from(s), p = out(reg) _,
           t = out(reg) _)
    });
}

unsafe fn chroma(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16) {
    let [up, cur, dn] = rows.map(<[u8]>::as_ptr);
    let out = dst.as_mut_ptr();
    // SAFETY: as above.
    strips(dst.len(), |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vle8.v v1, ({u})",
            "vle8.v v2, ({c})",
            "vle8.v v3, ({d})",
            "vwaddu.vv v4, v1, v3",
            "vwaddu.wv v4, v4, v2",
            "vwaddu.wv v4, v4, v2",
            "addi {p}, {u}, 1",
            "vle8.v v1, ({p})",
            "addi {p}, {c}, 1",
            "vle8.v v20, ({p})",
            "addi {p}, {d}, 1",
            "vle8.v v3, ({p})",
            "vwaddu.vv v6, v1, v3",
            "vwaddu.wv v6, v6, v20",
            "vwaddu.wv v6, v6, v20",
            "addi {p}, {u}, 2",
            "vle8.v v1, ({p})",
            "addi {p}, {c}, 2",
            "vle8.v v2, ({p})",
            "addi {p}, {d}, 2",
            "vle8.v v3, ({p})",
            "vwaddu.vv v8, v1, v3",
            "vwaddu.wv v8, v8, v2",
            "vwaddu.wv v8, v8, v2",
            "vsetvli zero, {n}, e16, m2, ta, mu",
            "vsll.vi v6, v6, 1",
            "vadd.vv v4, v4, v6",
            "vadd.vv v4, v4, v8",
            "vadd.vi v4, v4, 8",
            "vsrl.vi v4, v4, 4",
            "vzext.vf2 v10, v20",
            "vsub.vv v6, v4, v10",
            "vrsub.vi v14, v6, 0",
            "vmax.vv v14, v14, v6",
            "vsll.vi v14, v14, 2",
            "vmsleu.vx v0, v14, {thr}",
            scale_q8!("s"),
            "vadd.vv v10, v10, v12, v0.t",
            "vsetvli zero, {n}, e8, m1, ta, ma",
            "vnsrl.wi v1, v10, 0",
            "vse8.v v1, ({out})",
        ], left = in(reg) left, u = in(reg) up.add(i), c = in(reg) cur.add(i),
           d = in(reg) dn.add(i), out = in(reg) out.add(i), thr = in(reg) t as usize,
           s = in(reg) isize::from(s), p = out(reg) _, t = out(reg) _)
    });
}

unsafe fn sharpen(rows: [&[u8]; 3], dst: &mut [u8], amount: i16, thr: i16, limit: i16) {
    let [up, cur, dn] = rows.map(<[u8]>::as_ptr);
    let out = dst.as_mut_ptr();
    // SAFETY: as above.
    strips(dst.len(), |i, left| unsafe {
        rvv!([
            "vsetvli {n}, {left}, e8, m1, ta, ma",
            "vle8.v v1, ({u})",
            "vle8.v v2, ({c})",
            "vle8.v v3, ({d})",
            // v4 the corners, v8 the edges' sum.
            "vwaddu.vv v4, v1, v3",
            "vwaddu.vx v8, v2, zero",
            "addi {p}, {u}, 1",
            "vle8.v v1, ({p})",
            "addi {p}, {c}, 1",
            "vle8.v v20, ({p})",
            "addi {p}, {d}, 1",
            "vle8.v v3, ({p})",
            "vwaddu.wv v8, v8, v1",
            "vwaddu.wv v8, v8, v3",
            "addi {p}, {u}, 2",
            "vle8.v v1, ({p})",
            "addi {p}, {c}, 2",
            "vle8.v v2, ({p})",
            "addi {p}, {d}, 2",
            "vle8.v v3, ({p})",
            "vwaddu.wv v4, v4, v1",
            "vwaddu.wv v4, v4, v3",
            "vwaddu.wv v8, v8, v2",
            "vsetvli zero, {n}, e16, m2, ta, mu",
            "vsll.vi v8, v8, 1",
            "vadd.vv v4, v4, v8",
            "vzext.vf2 v10, v20",
            "vsll.vi v14, v10, 2",
            "vadd.vv v4, v4, v14",
            "vadd.vi v4, v4, 8",
            "vsrl.vi v4, v4, 4",
            // Detail, cored by the threshold, its sign put back.
            "vsub.vv v4, v10, v4",
            "vmslt.vx v0, v4, zero",
            "vrsub.vi v6, v4, 0",
            "vmax.vv v6, v6, v4",
            "vssubu.vx v6, v6, {thr}",
            "vrsub.vi v6, v6, 0, v0.t",
            scale_q8!("a"),
            "vmin.vx v12, v12, {lim}",
            "neg {t}, {lim}",
            "vmax.vx v12, v12, {t}",
            "vadd.vv v10, v10, v12",
            store_clamped!("v10", "out"),
        ], left = in(reg) left, u = in(reg) up.add(i), c = in(reg) cur.add(i),
           d = in(reg) dn.add(i), out = in(reg) out.add(i), thr = in(reg) thr as usize,
           lim = in(reg) limit as isize, a = in(reg) isize::from(amount), p = out(reg) _,
           t = out(reg) _)
    });
}

pub(in crate::isp) fn rgb_to_planes_row(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { rgb_to_planes(rgb, y, cb, cr, width) })
}

pub(in crate::isp) fn planes_to_rgb_row(
    planes: [&[u8]; 3],
    rgb: [&mut [u8]; 3],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { planes_to_rgb(planes, rgb, width) })
}

pub(in crate::isp) fn sigma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    table: &[u16; 256],
    s: i16,
) -> Option<(SimdBackend, usize)> {
    run(dst.len(), || unsafe { sigma(rows, dst, table, s) })
}

pub(in crate::isp) fn chroma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    t: i16,
    s: i16,
) -> Option<(SimdBackend, usize)> {
    run(dst.len(), || unsafe { chroma(rows, dst, t, s) })
}

pub(in crate::isp) fn sharpen_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
) -> Option<(SimdBackend, usize)> {
    run(dst.len(), || unsafe {
        sharpen(rows, dst, amount, thr, limit)
    })
}

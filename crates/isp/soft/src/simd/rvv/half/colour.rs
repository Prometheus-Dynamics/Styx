//! The colour kernels of the fp16 leaves: the bilinear demosaic's colour matrix and the
//! half-size quads' (see [`super`]), each channel through the tone curve into the output's
//! shape. The coefficients are splatted once per strip (`v8..v16`), per lane parity where
//! the bilinear sites alternate.

use super::{emit, quad_ptrs};
use crate::format::CfaPattern;
use crate::simd::half::{ColourCoeffs, ColourOut, H_16, H_HALF, HalfTone, quad_positions};

/// Bilinear demosaic, colour matrix and tone curve of one row (see
/// [`crate::simd::half::colour_row`]). Rows hold `width + 2` samples.
pub(in crate::simd) unsafe fn colour(
    rows: [&[u16]; 3],
    out: &mut ColourOut,
    width: usize,
    cc: &ColourCoeffs,
    tone: &HalfTone,
) -> usize {
    let [up, cur, dn] = rows.map(|r| r.as_ptr());
    let coef = cc.c.as_ptr().cast::<u16>();
    let ge = usize::from(cc.green_even);
    let (tb, ts) = (tone.base.as_ptr(), tone.slope.as_ptr());
    emit(out, width, |x, left, dst| unsafe {
        let (o, pk) = dst.ops();
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            // Lane parity in v30; v0 = odd lanes, for the coefficients of each parity.
            "vid.v v30",
            "vand.vi v30, v30, 1",
            "vmsne.vi v0, v30, 0",
            "lhu {t}, 0({cc})",
            "lhu {u}, 2({cc})",
            "vmv.v.x v8, {t}",
            "vmerge.vxm v8, v8, {u}, v0",
            "lhu {t}, 4({cc})",
            "lhu {u}, 6({cc})",
            "vmv.v.x v9, {t}",
            "vmerge.vxm v9, v9, {u}, v0",
            "lhu {t}, 8({cc})",
            "lhu {u}, 10({cc})",
            "vmv.v.x v10, {t}",
            "vmerge.vxm v10, v10, {u}, v0",
            "lhu {t}, 12({cc})",
            "lhu {u}, 14({cc})",
            "vmv.v.x v11, {t}",
            "vmerge.vxm v11, v11, {u}, v0",
            "lhu {t}, 16({cc})",
            "lhu {u}, 18({cc})",
            "vmv.v.x v12, {t}",
            "vmerge.vxm v12, v12, {u}, v0",
            "lhu {t}, 20({cc})",
            "lhu {u}, 22({cc})",
            "vmv.v.x v13, {t}",
            "vmerge.vxm v13, v13, {u}, v0",
            "lhu {t}, 24({cc})",
            "lhu {u}, 26({cc})",
            "vmv.v.x v14, {t}",
            "vmerge.vxm v14, v14, {u}, v0",
            "lhu {t}, 28({cc})",
            "lhu {u}, 30({cc})",
            "vmv.v.x v15, {t}",
            "vmerge.vxm v15, v15, {u}, v0",
            "lhu {t}, 32({cc})",
            "lhu {u}, 34({cc})",
            "vmv.v.x v16, {t}",
            "vmerge.vxm v16, v16, {u}, v0",
            // v0 = green lanes: even lanes when green is on even columns.
            "beqz {ge}, 7f",
            "vmnot.m v0, v0",
            "7:",
            "vmv.v.x v25, {h16}",
            // hs = cur[x] + cur[x+2] (v17), centre = cur[x+1] (v18), vs = up[x+1] + dn[x+1]
            // (v19), diag = up[x] + up[x+2] + dn[x] + dn[x+2] (v20).
            "vle16.v v17, ({cur})",
            "addi {t}, {cur}, 4",
            "vle16.v v18, ({t})",
            "vfadd.vv v17, v17, v18",
            "addi {t}, {cur}, 2",
            "vle16.v v18, ({t})",
            "addi {t}, {up}, 2",
            "vle16.v v19, ({t})",
            "addi {t}, {dn}, 2",
            "vle16.v v20, ({t})",
            "vfadd.vv v19, v19, v20",
            "vle16.v v20, ({up})",
            "addi {t}, {up}, 4",
            "vle16.v v21, ({t})",
            "vfadd.vv v20, v20, v21",
            "vle16.v v21, ({dn})",
            "addi {t}, {dn}, 4",
            "vle16.v v22, ({t})",
            "vfadd.vv v21, v21, v22",
            "vfadd.vv v20, v20, v21",
            // The three sums per lane: g (v21), xs (v22), ys (v23); green lanes take the
            // centre, hs, vs, the others hs + vs, centre, diag.
            "vfadd.vv v21, v17, v19",
            "vmerge.vvm v21, v21, v18, v0",
            "vmerge.vvm v22, v18, v17, v0",
            "vmerge.vvm v23, v20, v19, v0",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v21, v8",
            "vfmacc.vv v24, v22, v9",
            "vfmacc.vv v24, v23, v10",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v26"),
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v21, v11",
            "vfmacc.vv v24, v22, v12",
            "vfmacc.vv v24, v23, v13",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v27"),
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v21, v14",
            "vfmacc.vv v24, v22, v15",
            "vfmacc.vv v24, v23, v16",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v28"),
            store!(),
        ],
            left = in(reg) left,
            cur = in(reg) cur.wrapping_add(x),
            up = in(reg) up.wrapping_add(x),
            dn = in(reg) dn.wrapping_add(x),
            cc = in(reg) coef,
            ge = in(reg) ge,
            pk = in(reg) pk,
            o0 = in(reg) o[0],
            o1 = in(reg) o[1],
            o2 = in(reg) o[2],
            tb = in(reg) tb,
            ts = in(reg) ts,
            k76 = in(reg) 76usize,
            k48 = in(reg) 48usize,
            k128 = in(reg) 128usize,
            h16 = in(reg) usize::from(H_16),
            t = out(reg) _,
            u = out(reg) _,
        )
    });
    width
}

/// Half-size quads through the colour matrix `m` (fp16, row-major) and the tone curve (see
/// [`crate::simd::half::quad_colour_row`]). Inputs hold `2 * width` samples.
pub(in crate::simd) unsafe fn quad_colour(
    top: &[u16],
    bottom: &[u16],
    out: &mut ColourOut,
    width: usize,
    m: &[u16; 9],
    pattern: CfaPattern,
    tone: &HalfTone,
) -> usize {
    let pos = quad_positions(pattern);
    let (top, bottom) = (top.as_ptr(), bottom.as_ptr());
    let m = m.as_ptr();
    let (tb, ts) = (tone.base.as_ptr(), tone.slope.as_ptr());
    emit(out, width, |i, left, dst| unsafe {
        let [pr, pg1, pg2, pb] = quad_ptrs(top, bottom, i, pos);
        let (o, pk) = dst.ops();
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            // R (v17), G (v18: mean of the two greens), B (v20), one quad per lane.
            "vlse16.v v17, ({pr}), {s4}",
            "vlse16.v v18, ({pg1}), {s4}",
            "vlse16.v v19, ({pg2}), {s4}",
            "vlse16.v v20, ({pb}), {s4}",
            "vfadd.vv v18, v18, v19",
            "vmv.v.x v21, {half}",
            "vfmul.vv v18, v18, v21",
            // The matrix, splatted: v8..v16 row-major.
            "lhu {t}, 0({m})",
            "vmv.v.x v8, {t}",
            "lhu {t}, 2({m})",
            "vmv.v.x v9, {t}",
            "lhu {t}, 4({m})",
            "vmv.v.x v10, {t}",
            "lhu {t}, 6({m})",
            "vmv.v.x v11, {t}",
            "lhu {t}, 8({m})",
            "vmv.v.x v12, {t}",
            "lhu {t}, 10({m})",
            "vmv.v.x v13, {t}",
            "lhu {t}, 12({m})",
            "vmv.v.x v14, {t}",
            "lhu {t}, 14({m})",
            "vmv.v.x v15, {t}",
            "lhu {t}, 16({m})",
            "vmv.v.x v16, {t}",
            "vmv.v.x v25, {h16}",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v17, v8",
            "vfmacc.vv v24, v18, v9",
            "vfmacc.vv v24, v20, v10",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v26"),
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v17, v11",
            "vfmacc.vv v24, v18, v12",
            "vfmacc.vv v24, v20, v13",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v27"),
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vmv.v.x v24, {h16}",
            "vfmacc.vv v24, v17, v14",
            "vfmacc.vv v24, v18, v15",
            "vfmacc.vv v24, v20, v16",
            "vfmax.vv v24, v24, v25",
            tone!("v24", "v28"),
            store!(),
        ],
            left = in(reg) left,
            pr = in(reg) pr,
            pg1 = in(reg) pg1,
            pg2 = in(reg) pg2,
            pb = in(reg) pb,
            s4 = in(reg) 4usize,
            m = in(reg) m,
            pk = in(reg) pk,
            o0 = in(reg) o[0],
            o1 = in(reg) o[1],
            o2 = in(reg) o[2],
            tb = in(reg) tb,
            ts = in(reg) ts,
            k76 = in(reg) 76usize,
            k48 = in(reg) 48usize,
            k128 = in(reg) 128usize,
            half = in(reg) usize::from(H_HALF),
            h16 = in(reg) usize::from(H_16),
            t = out(reg) _,
        )
    });
    width
}

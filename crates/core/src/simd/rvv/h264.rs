//! H.264 leaves in RVV 1.0 assembly (`simd::h264`), exact against the scalar functions there.
//!
//! The transforms run one lane per 4x4 block of the grid (at most 16, which e16 at LMUL 2 and
//! e32 at LMUL 4 hold at the minimum VLEN of 128): samples gathered with indexed loads from
//! per-block offsets, the row pass's sums kept in a scratch table between the passes, levels
//! stored a block apart with strided stores. SAD takes a row of 16 bytes per step.

use core::arch::asm;

use super::super::SimdBackend;
use super::super::h264::{LEVEL_MAX, Quant4};

/// Lanes in the scratch tables (the most blocks in a grid).
const LANES: usize = 16;

/// Byte offset of each block's top-left sample in a plane with rows `stride` apart.
fn offsets((bw, bh): (usize, usize), stride: usize) -> [u32; LANES] {
    core::array::from_fn(|b| {
        if b < bw * bh {
            ((b / bw) * 4 * stride + (b % bw) * 4) as u32
        } else {
            0
        }
    })
}

/// `simd::h264::sad16` on vectors.
pub(in crate::simd) fn sad16(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize) -> Option<u32> {
    if !super::available() {
        return None;
    }
    let sum: usize;
    // SAFETY: the V extension; the dispatcher checked both blocks are 16 rows of 16 bytes.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetivli zero, 16, e16, m2, ta, ma",
            "vmv.v.i v12, 0",
            "li {cnt}, 16",
            "2:",
            "vsetivli zero, 16, e8, m1, ta, ma",
            "vle8.v v8, ({a})",
            "vle8.v v9, ({b})",
            "vmaxu.vv v10, v8, v9",
            "vminu.vv v11, v8, v9",
            "vsub.vv v10, v10, v11",
            "vwaddu.wv v12, v12, v10",
            "add {a}, {a}, {sa}",
            "add {b}, {b}, {sb}",
            "addi {cnt}, {cnt}, -1",
            "bnez {cnt}, 2b",
            "vsetivli zero, 16, e16, m2, ta, ma",
            "vmv.s.x v16, zero",
            "vredsum.vs v16, v12, v16",
            "vmv.x.s {sum}, v16",
            ".option pop",
            a = inout(reg) a.as_ptr() => _,
            b = inout(reg) b.as_ptr() => _,
            sa = in(reg) a_stride,
            sb = in(reg) b_stride,
            cnt = out(reg) _,
            sum = out(reg) sum,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v16") _, out("v17") _,
            options(nostack, readonly),
        );
    }
    // The sum is at most 16 · 16 · 255, positive in 16 bits' sign extension only above 32767.
    Some((sum & 0xffff) as u32)
}

/// `simd::h264::residual_quant` on vectors.
#[allow(clippy::too_many_arguments)]
pub(in crate::simd) fn residual_quant(
    src: &[u8],
    src_stride: usize,
    pred: &[u8],
    pred_stride: usize,
    grid: (usize, usize),
    q: &Quant4,
    ac_only: bool,
    dc: &mut [i32],
    levels: &mut [[i32; 16]],
) -> Option<SimdBackend> {
    let n = grid.0 * grid.1;
    if n > LANES || !super::available() {
        return None;
    }
    let (os, op) = (offsets(grid, src_stride), offsets(grid, pred_stride));
    let mut rows = [0i16; 16 * LANES];
    let mut coef = [0i16; 16 * LANES];
    for r in 0..4 {
        // SAFETY: the V extension; each lane's gather stays inside its block's row `r` of
        // `src` and `pred` (checked by the dispatcher); stores 4 rows of `n` lanes of `rows`.
        unsafe {
            asm!(
                ".option push",
                ".option arch, +v",
                "vsetvli zero, {n}, e32, m4, ta, ma",
                "vle32.v v16, ({os})",
                "vle32.v v20, ({op})",
                "vsetvli zero, {n}, e8, m1, ta, ma",
                "vluxei32.v v1, ({s}), v16",
                "vluxei32.v v2, ({p}), v20",
                "addi {s}, {s}, 1",
                "addi {p}, {p}, 1",
                "vluxei32.v v3, ({s}), v16",
                "vluxei32.v v4, ({p}), v20",
                "addi {s}, {s}, 1",
                "addi {p}, {p}, 1",
                "vluxei32.v v5, ({s}), v16",
                "vluxei32.v v6, ({p}), v20",
                "addi {s}, {s}, 1",
                "addi {p}, {p}, 1",
                "vluxei32.v v7, ({s}), v16",
                "vluxei32.v v8, ({p}), v20",
                "vsetvli zero, {n}, e16, m2, ta, ma",
                "vzext.vf2 v24, v1",
                "vzext.vf2 v10, v2",
                "vsub.vv v24, v24, v10",
                "vzext.vf2 v26, v3",
                "vzext.vf2 v10, v4",
                "vsub.vv v26, v26, v10",
                "vzext.vf2 v28, v5",
                "vzext.vf2 v10, v6",
                "vsub.vv v28, v28, v10",
                "vzext.vf2 v30, v7",
                "vzext.vf2 v10, v8",
                "vsub.vv v30, v30, v10",
                // a = s0 + s3, b = s1 + s2, c = s1 - s2, d = s0 - s3
                "vadd.vv v10, v24, v30",
                "vadd.vv v12, v26, v28",
                "vsub.vv v14, v26, v28",
                "vsub.vv v24, v24, v30",
                "vadd.vv v26, v10, v12",
                "vse16.v v26, ({t})",
                "addi {t}, {t}, 32",
                "vadd.vv v26, v24, v24",
                "vadd.vv v26, v26, v14",
                "vse16.v v26, ({t})",
                "addi {t}, {t}, 32",
                "vsub.vv v26, v10, v12",
                "vse16.v v26, ({t})",
                "addi {t}, {t}, 32",
                "vsub.vv v26, v24, v14",
                "vsub.vv v26, v26, v14",
                "vse16.v v26, ({t})",
                ".option pop",
                n = in(reg) n,
                os = in(reg) os.as_ptr(),
                op = in(reg) op.as_ptr(),
                s = inout(reg) src.as_ptr().add(r * src_stride) => _,
                p = inout(reg) pred.as_ptr().add(r * pred_stride) => _,
                t = inout(reg) rows.as_mut_ptr().add(r * 4 * LANES) => _,
                out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _,
                out("v7") _, out("v8") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
                out("v14") _, out("v15") _, out("v16") _, out("v17") _, out("v18") _, out("v19") _,
                out("v20") _, out("v21") _, out("v22") _, out("v23") _, out("v24") _, out("v25") _,
                out("v26") _, out("v27") _, out("v28") _, out("v29") _, out("v30") _, out("v31") _,
                options(nostack),
            );
        }
    }
    for u in 0..4 {
        // SAFETY: the V extension; reads column `u` of `rows`, writes column `u` of `coef`.
        unsafe {
            column_fwd(
                n,
                rows.as_ptr().add(u * LANES),
                coef.as_mut_ptr().add(u * LANES),
            );
        }
    }
    // SAFETY: the V extension; reads the 16 rows of `coef`, stores one level per lane per
    // position, the lanes 64 bytes (a block of levels) apart.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "li {cnt}, 16",
            "2:",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vle16.v v8, ({c})",
            "vsetvli zero, {n}, e32, m4, ta, mu",
            "vsext.vf2 v12, v8",
            "vrsub.vi v16, v12, 0",
            "vmax.vv v16, v16, v12",
            "lw {k}, 0({mf})",
            "vmul.vx v16, v16, {k}",
            "vadd.vx v16, v16, {f}",
            "vsra.vx v16, v16, {qbits}",
            "vmin.vx v16, v16, {lmax}",
            "vmslt.vx v0, v12, zero",
            "vrsub.vi v16, v16, 0, v0.t",
            "vsse32.v v16, ({lev}), {block}",
            "addi {c}, {c}, 32",
            "addi {mf}, {mf}, 4",
            "addi {lev}, {lev}, 4",
            "addi {cnt}, {cnt}, -1",
            "bnez {cnt}, 2b",
            ".option pop",
            n = in(reg) n,
            c = inout(reg) coef.as_ptr() => _,
            mf = inout(reg) q.mf.as_ptr() => _,
            f = in(reg) q.f,
            qbits = in(reg) q.qbits,
            lmax = in(reg) LEVEL_MAX,
            lev = inout(reg) levels.as_mut_ptr().cast::<i32>() => _,
            block = in(reg) 64usize,
            k = out(reg) _,
            cnt = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _, out("v12") _, out("v13") _, out("v14") _,
            out("v15") _, out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            options(nostack),
        );
    }
    for b in 0..n {
        dc[b] = i32::from(coef[b]);
        if ac_only {
            levels[b][0] = 0;
        }
    }
    Some(SimdBackend::Rvv)
}

/// The forward column pass of column `u`: four rows of `rows` (`4 · LANES` apart) to the
/// four coefficients `(v, u)` of `coef` (`4 · LANES` apart).
///
/// # Safety
/// The V extension; both tables hold 16 rows of `LANES` from the given column on.
unsafe fn column_fwd(n: usize, rows: *const i16, coef: *mut i16) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetvli zero, {n}, e16, m2, ta, ma",
            "vle16.v v24, ({t})",
            "add {t}, {t}, {pitch}",
            "vle16.v v26, ({t})",
            "add {t}, {t}, {pitch}",
            "vle16.v v28, ({t})",
            "add {t}, {t}, {pitch}",
            "vle16.v v30, ({t})",
            "vadd.vv v10, v24, v30",
            "vadd.vv v12, v26, v28",
            "vsub.vv v14, v26, v28",
            "vsub.vv v24, v24, v30",
            "vadd.vv v26, v10, v12",
            "vse16.v v26, ({c})",
            "add {c}, {c}, {pitch}",
            "vadd.vv v26, v24, v24",
            "vadd.vv v26, v26, v14",
            "vse16.v v26, ({c})",
            "add {c}, {c}, {pitch}",
            "vsub.vv v26, v10, v12",
            "vse16.v v26, ({c})",
            "add {c}, {c}, {pitch}",
            "vsub.vv v26, v24, v14",
            "vsub.vv v26, v26, v14",
            "vse16.v v26, ({c})",
            ".option pop",
            n = in(reg) n,
            t = inout(reg) rows => _,
            c = inout(reg) coef => _,
            pitch = in(reg) 4 * LANES * 2,
            out("v10") _, out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _,
            out("v30") _, out("v31") _,
            options(nostack),
        );
    }
}

/// The inverse pass over four values `stride` bytes apart at `src` (e32), results (rounded
/// when `round`) to four places `stride` apart at `dst`.
///
/// # Safety
/// The V extension; `n` lanes at each of the eight places.
unsafe fn inverse4(n: usize, src: *const i32, dst: *mut i32, stride: usize, round: bool) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vle32.v v8, ({s})",
            "add {s}, {s}, {pitch}",
            "vle32.v v12, ({s})",
            "add {s}, {s}, {pitch}",
            "vle32.v v16, ({s})",
            "add {s}, {s}, {pitch}",
            "vle32.v v20, ({s})",
            // e0 = s0 + s2, e1 = s0 - s2, e2 = (s1 >> 1) - s3, e3 = s1 + (s3 >> 1)
            "vadd.vv v24, v8, v16",
            "vsub.vv v8, v8, v16",
            "vsra.vi v28, v12, 1",
            "vsub.vv v28, v28, v20",
            "vsra.vi v20, v20, 1",
            "vadd.vv v12, v12, v20",
            "vadd.vv v16, v24, v12",
            "vadd.vx v16, v16, {r}",
            "vsra.vx v16, v16, {sh}",
            "vse32.v v16, ({d})",
            "add {d}, {d}, {pitch}",
            "vadd.vv v16, v8, v28",
            "vadd.vx v16, v16, {r}",
            "vsra.vx v16, v16, {sh}",
            "vse32.v v16, ({d})",
            "add {d}, {d}, {pitch}",
            "vsub.vv v16, v8, v28",
            "vadd.vx v16, v16, {r}",
            "vsra.vx v16, v16, {sh}",
            "vse32.v v16, ({d})",
            "add {d}, {d}, {pitch}",
            "vsub.vv v16, v24, v12",
            "vadd.vx v16, v16, {r}",
            "vsra.vx v16, v16, {sh}",
            "vse32.v v16, ({d})",
            ".option pop",
            n = in(reg) n,
            s = inout(reg) src => _,
            d = inout(reg) dst => _,
            pitch = in(reg) stride,
            r = in(reg) if round { 32 } else { 0 },
            sh = in(reg) if round { 6 } else { 0 },
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v14") _, out("v15") _, out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            out("v20") _, out("v21") _, out("v22") _, out("v23") _, out("v24") _, out("v25") _,
            out("v26") _, out("v27") _, out("v28") _, out("v29") _, out("v30") _, out("v31") _,
            options(nostack),
        );
    }
}

/// `simd::h264::recon` on vectors.
#[allow(clippy::too_many_arguments)]
pub(in crate::simd) fn recon(
    levels: &[[i32; 16]],
    scale: &[i32; 16],
    dc: Option<&[i32]>,
    pred: &[u8],
    pred_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    grid: (usize, usize),
) -> Option<SimdBackend> {
    let n = grid.0 * grid.1;
    if n > LANES || !super::available() {
        return None;
    }
    // Dequantised coefficients, then the row pass, then the residual: position-major tables.
    let mut d = [0i32; 16 * LANES];
    // SAFETY: the V extension; loads each position's level from every block (64 bytes
    // apart), stores `n` lanes of each of the 16 rows of `d`.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "li {cnt}, 16",
            "2:",
            "vsetvli zero, {n}, e32, m4, ta, ma",
            "vlse32.v v8, ({lev}), {block}",
            "lw {k}, 0({sc})",
            "vmul.vx v8, v8, {k}",
            "vse32.v v8, ({d})",
            "addi {lev}, {lev}, 4",
            "addi {sc}, {sc}, 4",
            "addi {d}, {d}, {row}",
            "addi {cnt}, {cnt}, -1",
            "bnez {cnt}, 2b",
            ".option pop",
            n = in(reg) n,
            lev = inout(reg) levels.as_ptr().cast::<i32>() => _,
            block = in(reg) 64usize,
            sc = inout(reg) scale.as_ptr() => _,
            d = inout(reg) d.as_mut_ptr() => _,
            row = const LANES * 4,
            k = out(reg) _,
            cnt = out(reg) _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            options(nostack),
        );
    }
    if let Some(dc) = dc {
        d[..n].copy_from_slice(&dc[..n]);
    }
    let mut t = [0i32; 16 * LANES];
    for y in 0..4 {
        // SAFETY: the V extension; row `y`'s four positions in, four positions out.
        unsafe {
            inverse4(
                n,
                d.as_ptr().add(y * 4 * LANES),
                t.as_mut_ptr().add(y * 4 * LANES),
                LANES * 4,
                false,
            )
        };
    }
    for x in 0..4 {
        // SAFETY: as above, column `x` (positions `x`, `4 + x`, …).
        unsafe {
            inverse4(
                n,
                t.as_ptr().add(x * LANES),
                d.as_mut_ptr().add(x * LANES),
                4 * LANES * 4,
                true,
            )
        };
    }
    let (op, od) = (offsets(grid, pred_stride), offsets(grid, dst_stride));
    for i in 0..16 {
        let (c, r) = (i & 3, i >> 2);
        // SAFETY: the V extension; each lane gathers from its block's sample `i` of `pred`
        // and scatters to the same sample of `dst`, both inside (checked by the dispatcher).
        unsafe {
            asm!(
                ".option push",
                ".option arch, +v",
                "vsetvli zero, {n}, e32, m4, ta, ma",
                "vle32.v v16, ({op})",
                "vle32.v v20, ({od})",
                "vle32.v v8, ({r})",
                "vsetvli zero, {n}, e8, m1, ta, ma",
                "vluxei32.v v1, ({p}), v16",
                "vsetvli zero, {n}, e32, m4, ta, ma",
                "vzext.vf4 v12, v1",
                "vadd.vv v8, v8, v12",
                "vmax.vx v8, v8, zero",
                "vmin.vx v8, v8, {max}",
                "vsetvli zero, {n}, e16, m2, ta, ma",
                "vnsrl.wi v2, v8, 0",
                "vsetvli zero, {n}, e8, m1, ta, ma",
                "vnsrl.wi v1, v2, 0",
                "vsuxei32.v v1, ({dp}), v20",
                ".option pop",
                n = in(reg) n,
                op = in(reg) op.as_ptr(),
                od = in(reg) od.as_ptr(),
                r = in(reg) d.as_ptr().add(i * LANES),
                p = in(reg) pred.as_ptr().add(r * pred_stride + c),
                dp = in(reg) dst.as_mut_ptr().add(r * dst_stride + c),
                max = in(reg) 255usize,
                out("v1") _, out("v2") _, out("v3") _, out("v8") _, out("v9") _, out("v10") _,
                out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _,
                out("v21") _, out("v22") _, out("v23") _,
                options(nostack),
            );
        }
    }
    Some(SimdBackend::Rvv)
}

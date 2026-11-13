//! The output picture's stages in 32-bit Arm NEON assembly, 8 samples a step in 16-bit lanes.
//!
//! # Safety (every function)
//! The CPU must have NEON, and the slices must hold `steps` blocks of input and output; the
//! filters' rows hold two more samples than `dst`, read from the column before it.

use super::{SimdBackend, blocks, lanes16};
use crate::isp::post::RCP;

/// A [`RCP`] table of low or high bytes as the two doublewords `vtbl` takes.
fn rcp_table(shift: u32) -> [f64; 2] {
    let word = |from: usize| {
        (0..8).rev().fold(0u64, |w, i| {
            (w << 8) | u64::from(RCP.get(from + i).copied().unwrap_or(0) >> shift & 0xFF)
        })
    };
    [f64::from_bits(word(0)), f64::from_bits(word(8))]
}

/// Sum and count one sigma-filter neighbour (`d2`): `q0` centres, `q8` thresholds, `q9` sum, `q10`
/// count.
macro_rules! neighbour {
    ($load:literal) => {
        concat!(
            $load,
            "\n",
            "vmovl.u8 q1, d2\n",
            "vabd.u16 q11, q1, q0\n",
            "vshl.u16 q11, q11, #2\n",
            "vcge.u16 q11, q8, q11\n",
            "vand q1, q1, q11\n",
            "vadd.i16 q9, q9, q1\n",
            "vsub.i16 q10, q10, q11\n",
        )
    };
}

unsafe fn rgb_to_planes(rgb: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        neon!([
            "vmov.i8 d20, #77",
            "vmov.i8 d21, #150",
            "vmov.i8 d22, #29",
            "vmov.i8 d23, #43",
            "vmov.i8 d24, #85",
            "vmov.i8 d25, #107",
            "vmov.i8 d26, #21",
            "vmov.i16 q4, #128",
        ], [
            "vld3.8 {{d0, d1, d2}}, [{s}]!",
            "vmull.u8 q9, d0, d20",
            "vmlal.u8 q9, d1, d21",
            "vmlal.u8 q9, d2, d22",
            "vrshrn.u16 d18, q9, #8",
            "vst1.8 {{d18}}, [{y}]!",
            // The sums reach 32640 and only the rounding can pass i16: saturating it gives
            // 127 + 128 = 255, which the clamp would.
            "vshll.u8 q9, d2, #7",
            "vmlsl.u8 q9, d0, d23",
            "vmlsl.u8 q9, d1, d24",
            "vqadd.s16 q9, q9, q4",
            "vshr.s16 q9, q9, #8",
            "vadd.i16 q9, q9, q4",
            "vqmovun.s16 d18, q9",
            "vst1.8 {{d18}}, [{cb}]!",
            "vshll.u8 q9, d0, #7",
            "vmlsl.u8 q9, d1, d25",
            "vmlsl.u8 q9, d2, d26",
            "vqadd.s16 q9, q9, q4",
            "vshr.s16 q9, q9, #8",
            "vadd.i16 q9, q9, q4",
            "vqmovun.s16 d18, q9",
            "vst1.8 {{d18}}, [{cr}]!",
        ], [],
        s = inout(reg) rgb.as_ptr() => _,
        y = inout(reg) y.as_mut_ptr() => _,
        cb = inout(reg) cb.as_mut_ptr() => _,
        cr = inout(reg) cr.as_mut_ptr() => _,
        n = inout(reg) steps => _)
    }
}

unsafe fn planes_to_rgb(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], steps: usize) {
    let [r, g, b] = rgb;
    // SAFETY: per the module.
    unsafe {
        // `359 = 256 + 103`, `-183 = -256 + 73`, `454 = 512 - 58` keep the products in 16 bits.
        neon!([
            "vmov.i16 q12, #128",
            "vmov.i16 q10, #103",
            "vmov.i16 q11, #73",
            "vmov.i16 q13, #88",
            "vmov.i16 q4, #58",
        ], [
            "vld1.8 {{d0}}, [{y}]!",
            "vld1.8 {{d1}}, [{cb}]!",
            "vld1.8 {{d2}}, [{cr}]!",
            "vmovl.u8 q2, d0",
            "vmovl.u8 q3, d1",
            "vmovl.u8 q8, d2",
            "vsub.i16 q3, q3, q12",
            "vsub.i16 q8, q8, q12",
            "vmul.i16 q9, q8, q10",
            "vadd.i16 q9, q9, q12",
            "vshr.s16 q9, q9, #8",
            "vadd.i16 q9, q9, q8",
            "vadd.i16 q9, q9, q2",
            "vqmovun.s16 d0, q9",
            "vst1.8 {{d0}}, [{r}]!",
            "vmul.i16 q9, q8, q11",
            "vmls.i16 q9, q3, q13",
            "vadd.i16 q9, q9, q12",
            "vshr.s16 q9, q9, #8",
            "vsub.i16 q9, q9, q8",
            "vadd.i16 q9, q9, q2",
            "vqmovun.s16 d0, q9",
            "vst1.8 {{d0}}, [{g}]!",
            "vmov q9, q12",
            "vmls.i16 q9, q3, q4",
            "vshr.s16 q9, q9, #8",
            "vadd.i16 q9, q9, q3",
            "vadd.i16 q9, q9, q3",
            "vadd.i16 q9, q9, q2",
            "vqmovun.s16 d0, q9",
            "vst1.8 {{d0}}, [{b}]!",
        ], [],
        y = inout(reg) planes[0].as_ptr() => _,
        cb = inout(reg) planes[1].as_ptr() => _,
        cr = inout(reg) planes[2].as_ptr() => _,
        r = inout(reg) r.as_mut_ptr() => _,
        g = inout(reg) g.as_mut_ptr() => _,
        b = inout(reg) b.as_mut_ptr() => _,
        n = inout(reg) steps => _)
    }
}

/// One step: the thresholds of its 8 centres in `lanes`, and each row from the column before.
unsafe fn sigma_step(
    rows: [*const u8; 3],
    dst: *mut u8,
    lanes: &[u16; 8],
    s: i16,
    tables: &[[f64; 2]; 2],
) {
    // SAFETY: per the module.
    unsafe {
        neon!([
            "vdup.16 d27, {s}",
            "vld1.16 {{d16, d17}}, [{th}]",
            // The middle row first: the centre between its two neighbours.
            "vld1.8 {{d24}}, [{c}], {one}",
            "vld1.8 {{d0}}, [{c}], {one}",
            "vld1.8 {{d25}}, [{c}]",
            "vmovl.u8 q0, d0",
            "vmov q9, q0",
            "vmov.i16 q10, #1",
        ], [
            neighbour!("vld1.8 {{d2}}, [{u}], {one}"),
            neighbour!("vld1.8 {{d2}}, [{u}], {one}"),
            neighbour!("vld1.8 {{d2}}, [{u}]"),
            neighbour!("vmov d2, d24"),
            neighbour!("vmov d2, d25"),
            neighbour!("vld1.8 {{d2}}, [{d}], {one}"),
            neighbour!("vld1.8 {{d2}}, [{d}], {one}"),
            neighbour!("vld1.8 {{d2}}, [{d}]"),
            // The count looks up its reciprocal, a byte at a time.
            "vmovn.i16 d6, q10",
            "vtbl.8 d7, {{d28, d29}}, d6",
            "vtbl.8 d10, {{d30, d31}}, d6",
            "vmovl.u8 q6, d7",
            "vshll.u8 q7, d10, #8",
            "vorr q6, q6, q7",
            "vmull.u16 q4, d18, d12",
            "vmull.u16 q5, d19, d13",
            "vrshrn.i32 d18, q4, #16",
            "vrshrn.i32 d19, q5, #16",
            "vsub.i16 q10, q9, q0",
            "vmull.s16 q4, d20, d27",
            "vmull.s16 q5, d21, d27",
            "vrshrn.i32 d20, q4, #8",
            "vrshrn.i32 d21, q5, #8",
            "vadd.i16 q0, q0, q10",
            "vqmovun.s16 d0, q0",
            "vst1.8 {{d0}}, [{out}]",
        ], [],
        u = inout(reg) rows[0] => _,
        c = inout(reg) rows[1] => _,
        d = inout(reg) rows[2] => _,
        out = in(reg) dst,
        th = in(reg) lanes.as_ptr(),
        one = in(reg) 1usize,
        s = in(reg) i32::from(s),
        n = inout(reg) 1usize => _,
        inout("d28") tables[0][0] => _, inout("d29") tables[0][1] => _,
        inout("d30") tables[1][0] => _, inout("d31") tables[1][1] => _)
    }
}

/// 8 samples a step; `d28` threshold, `d29` strength in all lanes.
unsafe fn chroma(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16, steps: usize) {
    // SAFETY: per the module.
    unsafe {
        neon!(["vdup.16 q13, d28[0]"], [
            "vld1.8 {{d0}}, [{u}], {one}",
            "vld1.8 {{d1}}, [{c}], {one}",
            "vld1.8 {{d2}}, [{d}], {one}",
            "vaddl.u8 q9, d0, d2",
            "vaddw.u8 q9, q9, d1",
            "vaddw.u8 q9, q9, d1",
            "vld1.8 {{d0}}, [{u}], {one}",
            "vld1.8 {{d3}}, [{c}], {one}",
            "vld1.8 {{d2}}, [{d}], {one}",
            "vaddl.u8 q2, d0, d2",
            "vaddw.u8 q2, q2, d3",
            "vaddw.u8 q2, q2, d3",
            "vadd.i16 q2, q2, q2",
            "vadd.i16 q9, q9, q2",
            "vld1.8 {{d0}}, [{u}]",
            "vld1.8 {{d1}}, [{c}]",
            "vld1.8 {{d2}}, [{d}]",
            "vaddl.u8 q2, d0, d2",
            "vaddw.u8 q2, q2, d1",
            "vaddw.u8 q2, q2, d1",
            "vadd.i16 q9, q9, q2",
            "vrshr.u16 q9, q9, #4",
            "vmovl.u8 q3, d3",
            "vsub.i16 q10, q9, q3",
            "vabs.s16 q11, q10",
            "vshl.u16 q11, q11, #2",
            "vcge.u16 q11, q13, q11",
            "vmull.s16 q4, d20, d29",
            "vmull.s16 q5, d21, d29",
            "vrshrn.i32 d20, q4, #8",
            "vrshrn.i32 d21, q5, #8",
            "vand q10, q10, q11",
            "vadd.i16 q3, q3, q10",
            "vqmovun.s16 d0, q3",
            "vst1.8 {{d0}}, [{out}]!",
            "add {u}, {u}, #6",
            "add {c}, {c}, #6",
            "add {d}, {d}, #6",
        ], [],
        u = inout(reg) rows[0].as_ptr() => _,
        c = inout(reg) rows[1].as_ptr() => _,
        d = inout(reg) rows[2].as_ptr() => _,
        out = inout(reg) dst.as_mut_ptr() => _,
        one = in(reg) 1usize,
        n = inout(reg) steps => _,
        inout("d28") lanes16([t as u16; 4]) => _, inout("d29") lanes16([s as u16; 4]) => _)
    }
}

/// 8 samples a step; `d28` threshold, `d29` limit, `d30` amount in all lanes.
unsafe fn sharpen(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
    steps: usize,
) {
    // SAFETY: per the module.
    unsafe {
        neon!([
            "vdup.16 q6, d28[0]",
            "vdup.16 q7, d29[0]",
            "vneg.s16 q12, q7",
        ], [
            // q2 the corners, q8 the edges' sum.
            "vld1.8 {{d0}}, [{u}], {one}",
            "vld1.8 {{d1}}, [{c}], {one}",
            "vld1.8 {{d2}}, [{d}], {one}",
            "vaddl.u8 q2, d0, d2",
            "vmovl.u8 q8, d1",
            "vld1.8 {{d0}}, [{u}], {one}",
            "vld1.8 {{d6}}, [{c}], {one}",
            "vld1.8 {{d2}}, [{d}], {one}",
            "vaddw.u8 q8, q8, d0",
            "vaddw.u8 q8, q8, d2",
            "vld1.8 {{d0}}, [{u}]",
            "vld1.8 {{d1}}, [{c}]",
            "vld1.8 {{d2}}, [{d}]",
            "vaddw.u8 q2, q2, d0",
            "vaddw.u8 q2, q2, d2",
            "vaddw.u8 q8, q8, d1",
            "vmovl.u8 q9, d6",
            "vshl.i16 q8, q8, #1",
            "vadd.i16 q2, q2, q8",
            "vshl.i16 q10, q9, #2",
            "vadd.i16 q2, q2, q10",
            "vrshr.u16 q2, q2, #4",
            // Detail, cored by the threshold, its sign put back.
            "vsub.i16 q10, q9, q2",
            "vshr.s16 q11, q10, #15",
            "vabs.s16 q8, q10",
            "vqsub.u16 q8, q8, q6",
            "veor q8, q8, q11",
            "vsub.i16 q8, q8, q11",
            "vmull.s16 q4, d16, d30",
            "vmull.s16 q5, d17, d30",
            "vrshrn.i32 d16, q4, #8",
            "vrshrn.i32 d17, q5, #8",
            "vmin.s16 q8, q8, q7",
            "vmax.s16 q8, q8, q12",
            "vadd.i16 q9, q9, q8",
            "vqmovun.s16 d0, q9",
            "vst1.8 {{d0}}, [{out}]!",
            "add {u}, {u}, #6",
            "add {c}, {c}, #6",
            "add {d}, {d}, #6",
        ], [],
        u = inout(reg) rows[0].as_ptr() => _,
        c = inout(reg) rows[1].as_ptr() => _,
        d = inout(reg) rows[2].as_ptr() => _,
        out = inout(reg) dst.as_mut_ptr() => _,
        one = in(reg) 1usize,
        n = inout(reg) steps => _,
        inout("d28") lanes16([thr as u16; 4]) => _, inout("d29") lanes16([limit as u16; 4]) => _,
        inout("d30") lanes16([amount as u16; 4]) => _)
    }
}

pub(in crate::isp) fn rgb_to_planes_row(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { rgb_to_planes(rgb, y, cb, cr, n) })
}

pub(in crate::isp) fn planes_to_rgb_row(
    planes: [&[u8]; 3],
    rgb: [&mut [u8]; 3],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { planes_to_rgb(planes, rgb, n) })
}

pub(in crate::isp) fn sigma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    table: &[u16; 256],
    s: i16,
) -> Option<(SimdBackend, usize)> {
    blocks(dst.len(), 8, |steps| {
        let tables = [rcp_table(0), rcp_table(8)];
        let mut lanes = [0u16; 8];
        for step in 0..steps {
            let x = 8 * step;
            for (i, l) in lanes.iter_mut().enumerate() {
                *l = table[usize::from(rows[1][x + 1 + i])];
            }
            let at = rows.map(|r| r[x..].as_ptr());
            unsafe { sigma_step(at, dst[x..].as_mut_ptr(), &lanes, s, &tables) };
        }
    })
}

pub(in crate::isp) fn chroma_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    t: i16,
    s: i16,
) -> Option<(SimdBackend, usize)> {
    blocks(dst.len(), 8, |n| unsafe { chroma(rows, dst, t, s, n) })
}

pub(in crate::isp) fn sharpen_row(
    rows: [&[u8]; 3],
    dst: &mut [u8],
    amount: i16,
    thr: i16,
    limit: i16,
) -> Option<(SimdBackend, usize)> {
    blocks(dst.len(), 8, |n| unsafe {
        sharpen(rows, dst, amount, thr, limit, n)
    })
}

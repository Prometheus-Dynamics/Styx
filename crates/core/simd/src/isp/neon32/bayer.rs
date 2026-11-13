//! Mosaic-domain kernels in 32-bit Arm NEON assembly: unpacking, front end, demosaic, luma,
//! quads, statistics. Each runs `steps` blocks of 8 pixels.
//!
//! # Safety (every function)
//! The CPU must have NEON, and the slices must hold `steps` blocks of input and output (with
//! the neighbours each scalar kernel documents).

use super::{lanes16, neon};
use crate::isp::RowKind;
use styx_format::CfaOrder;

/// `vtbl` indices of the high and low bits of 8 pixels (from 10 or 12 bytes), the per-lane
/// shifts of the low bits, the shift of the high bits and the low bits' mask.
#[repr(C, align(16))]
struct Unpack {
    high: [u8; 16],
    low: [u8; 16],
    low_shift: [i16; 8],
    high_shift: [i16; 8],
    mask: [u16; 8],
}

const RAW10: Unpack = Unpack {
    high: [
        0, 255, 1, 255, 2, 255, 3, 255, 5, 255, 6, 255, 7, 255, 8, 255,
    ],
    low: [
        4, 255, 4, 255, 4, 255, 4, 255, 9, 255, 9, 255, 9, 255, 9, 255,
    ],
    low_shift: [0, -2, -4, -6, 0, -2, -4, -6],
    high_shift: [2; 8],
    mask: [3; 8],
};

const RAW12: Unpack = Unpack {
    high: [
        0, 255, 1, 255, 3, 255, 4, 255, 6, 255, 7, 255, 9, 255, 10, 255,
    ],
    low: [
        2, 255, 2, 255, 5, 255, 5, 255, 8, 255, 8, 255, 11, 255, 11, 255,
    ],
    low_shift: [0, -4, 0, -4, 0, -4, 0, -4],
    high_shift: [4; 8],
    mask: [0xF; 8],
};

/// 8 pixels from 10 (RAW10) or 12 (RAW12) bytes a step, read without going past them.
pub(super) unsafe fn unpack(src: &[u8], dst: &mut [u16], steps: usize, raw12: bool) {
    let table = if raw12 { &RAW12 } else { &RAW10 };
    macro_rules! unpack {
        ($tail:literal) => {
            neon!([
                "vld1.8 {{d24, d25, d26, d27}}, [{k}]!",
                "vld1.16 {{d20, d21, d22, d23}}, [{k}]!",
                "vld1.16 {{d18, d19}}, [{k}]",
            ], [
                "vld1.8 {{d0}}, [{s}]!",
                $tail,
                "vtbl.8 d4, {{d0, d1}}, d24",
                "vtbl.8 d5, {{d0, d1}}, d25",
                "vtbl.8 d6, {{d0, d1}}, d26",
                "vtbl.8 d7, {{d0, d1}}, d27",
                "vshl.u16 q2, q2, q11",
                "vshl.u16 q3, q3, q10",
                "vand q3, q3, q9",
                "vorr q2, q2, q3",
                "vst1.16 {{d4, d5}}, [{d}]!",
            ], [],
            k = inout(reg) core::ptr::from_ref(table) => _,
            s = inout(reg) src.as_ptr() => _,
            d = inout(reg) dst.as_mut_ptr() => _,
            n = inout(reg) steps => _)
        };
    }
    // SAFETY: per the module; the bytes past each group's last one are never looked up.
    unsafe {
        if raw12 {
            unpack!("vld1.32 {{d1[0]}}, [{s}]!");
        } else {
            unpack!("vld1.16 {{d1[0]}}, [{s}]!");
        }
    }
}

pub(super) unsafe fn front(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    steps: usize,
) {
    let bl = lanes16([black[0], black[1], black[0], black[1]]);
    let sh = lanes16([shift as u16; 4]);
    // SAFETY: per the module.
    unsafe {
        neon!([
            "vmvn.i16 q8, #0xF000",
        ], [
            "vld1.16 {{d0, d1}}, [{r}]",
            "vld1.16 {{d2, d3}}, [{g}]!",
            "vqsub.u16 q0, q0, q14",
            "vshl.u16 q0, q0, q15",
            "vmull.u16 q2, d0, d2",
            "vmull.u16 q3, d1, d3",
            "vshrn.u32 d0, q2, #16",
            "vshrn.u32 d1, q3, #16",
            "vmin.u16 q0, q0, q8",
            "vst1.16 {{d0, d1}}, [{r}]!",
        ], [],
        r = inout(reg) row.as_mut_ptr() => _,
        g = inout(reg) gains.as_ptr() => _,
        n = inout(reg) steps => _,
        inout("d28") bl => _, inout("d29") bl => _,
        inout("d30") sh => _, inout("d31") sh => _);
    }
}

/// The green columns of a row kind as a lane mask.
fn green_mask(kind: RowKind) -> f64 {
    lanes16(if kind.green_even {
        [0xFFFF, 0, 0xFFFF, 0]
    } else {
        [0, 0xFFFF, 0, 0xFFFF]
    })
}

/// The `(X, Y)` output rows of a row kind (see [`RowKind`]).
fn xy<'a>(r: &'a mut [u16], b: &'a mut [u16], kind: RowKind) -> (&'a mut [u16], &'a mut [u16]) {
    if kind.x_is_red { (r, b) } else { (b, r) }
}

pub(super) unsafe fn demosaic_bilinear(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    steps: usize,
    kind: RowKind,
) {
    let [up, mid, dn] = rows;
    let [r, g, b] = out;
    let (xo, yo) = xy(r, b, kind);
    let mask = green_mask(kind);
    // SAFETY: per the module; each step reads columns -1..=8 of its block.
    unsafe {
        neon!([], [
            "vld1.16 {{d0, d1}}, [{up}]",
            "add {up}, {up}, #2",
            "vld1.16 {{d2, d3}}, [{up}]",
            "add {up}, {up}, #2",
            "vld1.16 {{d4, d5}}, [{up}]",
            "add {up}, {up}, #12",
            "vld1.16 {{d6, d7}}, [{mid}]",
            "add {mid}, {mid}, #2",
            "vld1.16 {{d8, d9}}, [{mid}]",
            "add {mid}, {mid}, #2",
            "vld1.16 {{d10, d11}}, [{mid}]",
            "add {mid}, {mid}, #12",
            "vld1.16 {{d12, d13}}, [{dn}]",
            "add {dn}, {dn}, #2",
            "vld1.16 {{d14, d15}}, [{dn}]",
            "add {dn}, {dn}, #2",
            "vld1.16 {{d16, d17}}, [{dn}]",
            "add {dn}, {dn}, #12",
            // Diagonal mean (q0), cross mean (q9).
            "vadd.i16 q0, q0, q2",
            "vadd.i16 q6, q6, q8",
            "vadd.i16 q0, q0, q6",
            "vrshr.u16 q0, q0, #2",
            "vadd.i16 q9, q3, q5",
            "vadd.i16 q10, q1, q7",
            "vadd.i16 q9, q9, q10",
            "vrshr.u16 q9, q9, #2",
            "vbit q9, q4, q15",
            "vst1.16 {{d18, d19}}, [{g}]!",
            "vrhadd.u16 q3, q3, q5",
            "vbif q3, q4, q15",
            "vst1.16 {{d6, d7}}, [{x}]!",
            "vrhadd.u16 q1, q1, q7",
            "vbit q0, q1, q15",
            "vst1.16 {{d0, d1}}, [{y}]!",
        ], [],
        up = inout(reg) up.as_ptr() => _,
        mid = inout(reg) mid.as_ptr() => _,
        dn = inout(reg) dn.as_ptr() => _,
        g = inout(reg) g.as_mut_ptr() => _,
        x = inout(reg) xo.as_mut_ptr() => _,
        y = inout(reg) yo.as_mut_ptr() => _,
        n = inout(reg) steps => _,
        inout("d30") mask => _, inout("d31") mask => _);
    }
}

pub(super) unsafe fn demosaic_mhc(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    steps: usize,
    kind: RowKind,
) {
    let [u2, u1, c0, d1, d2] = rows;
    let [r, g, b] = out;
    let (xo, yo) = xy(r, b, kind);
    let mask = green_mask(kind);
    // The kernel weights, for by-lane multiplies from d6 and d7.
    let (k0, k1) = (lanes16([10, 8, 2, 4]), lanes16([12, 3, 0, 0]));
    // SAFETY: per the module; each step reads columns -2..=9 of its block (the outer rows only
    // column 0..=7, the inner ones -1..=8).
    unsafe {
        neon!([
            "vmov d6, d28",
            "vmov d7, d29",
            "vmvn.i16 q13, #0xF000",
        ], [
            // The centre row at -2..=2: far_h (q0), near_h (q1), c (q2).
            "vld1.16 {{d0, d1}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d2, d3}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d4, d5}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d8, d9}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d10, d11}}, [{c0}]",
            "add {c0}, {c0}, #8",
            "vadd.i16 q0, q0, q5",
            "vadd.i16 q1, q1, q4",
            // far_v (q4).
            "vld1.16 {{d8, d9}}, [{u2}]!",
            "vld1.16 {{d10, d11}}, [{d2}]!",
            "vadd.i16 q4, q4, q5",
            // diag (q5), near_v (q6).
            "vld1.16 {{d10, d11}}, [{u1}]",
            "add {u1}, {u1}, #2",
            "vld1.16 {{d12, d13}}, [{u1}]",
            "add {u1}, {u1}, #2",
            "vld1.16 {{d14, d15}}, [{u1}]",
            "add {u1}, {u1}, #12",
            "vadd.i16 q5, q5, q7",
            "vld1.16 {{d14, d15}}, [{d1}]",
            "add {d1}, {d1}, #2",
            "vld1.16 {{d16, d17}}, [{d1}]",
            "add {d1}, {d1}, #2",
            "vld1.16 {{d18, d19}}, [{d1}]",
            "add {d1}, {d1}, #12",
            "vadd.i16 q7, q7, q9",
            "vadd.i16 q5, q5, q7",
            "vadd.i16 q6, q6, q8",
            // X at green sites: 10 c + 8 near_h - 2 (far_h + diag) + far_v.
            "vadd.i16 q7, q0, q5",
            "vmull.s16 q8, d4, d6[0]",
            "vmlal.s16 q8, d2, d6[1]",
            "vmlsl.s16 q8, d14, d6[2]",
            "vaddw.s16 q8, q8, d8",
            "vmull.s16 q9, d5, d6[0]",
            "vmlal.s16 q9, d3, d6[1]",
            "vmlsl.s16 q9, d15, d6[2]",
            "vaddw.s16 q9, q9, d9",
            "vqrshrun.s32 d16, q8, #4",
            "vqrshrun.s32 d17, q9, #4",
            "vmin.u16 q8, q8, q13",
            "vbif q8, q2, q15",
            "vst1.16 {{d16, d17}}, [{x}]!",
            // Y at green sites: 10 c + 8 near_v - 2 (far_v + diag) + far_h.
            "vadd.i16 q7, q4, q5",
            "vmull.s16 q8, d4, d6[0]",
            "vmlal.s16 q8, d12, d6[1]",
            "vmlsl.s16 q8, d14, d6[2]",
            "vaddw.s16 q8, q8, d0",
            "vmull.s16 q9, d5, d6[0]",
            "vmlal.s16 q9, d13, d6[1]",
            "vmlsl.s16 q9, d15, d6[2]",
            "vaddw.s16 q9, q9, d1",
            "vqrshrun.s32 d16, q8, #4",
            "vqrshrun.s32 d17, q9, #4",
            "vmin.u16 q8, q8, q13",
            // Y elsewhere: 12 c + 4 diag - 3 (far_h + far_v).
            "vadd.i16 q7, q0, q4",
            "vmull.s16 q10, d4, d7[0]",
            "vmlal.s16 q10, d10, d6[3]",
            "vmlsl.s16 q10, d14, d7[1]",
            "vmull.s16 q11, d5, d7[0]",
            "vmlal.s16 q11, d11, d6[3]",
            "vmlsl.s16 q11, d15, d7[1]",
            "vqrshrun.s32 d20, q10, #4",
            "vqrshrun.s32 d21, q11, #4",
            "vmin.u16 q10, q10, q13",
            "vbit q10, q8, q15",
            "vst1.16 {{d20, d21}}, [{y}]!",
            // Green elsewhere: 8 c + 4 (near_h + near_v) - 2 (far_h + far_v).
            "vadd.i16 q1, q1, q6",
            "vmull.s16 q8, d4, d6[1]",
            "vmlal.s16 q8, d2, d6[3]",
            "vmlsl.s16 q8, d14, d6[2]",
            "vmull.s16 q9, d5, d6[1]",
            "vmlal.s16 q9, d3, d6[3]",
            "vmlsl.s16 q9, d15, d6[2]",
            "vqrshrun.s32 d16, q8, #4",
            "vqrshrun.s32 d17, q9, #4",
            "vmin.u16 q8, q8, q13",
            "vbit q8, q2, q15",
            "vst1.16 {{d16, d17}}, [{g}]!",
        ], [],
        u2 = inout(reg) u2.as_ptr().add(2) => _,
        u1 = inout(reg) u1.as_ptr().add(1) => _,
        c0 = inout(reg) c0.as_ptr() => _,
        d1 = inout(reg) d1.as_ptr().add(1) => _,
        d2 = inout(reg) d2.as_ptr().add(2) => _,
        g = inout(reg) g.as_mut_ptr() => _,
        x = inout(reg) xo.as_mut_ptr() => _,
        y = inout(reg) yo.as_mut_ptr() => _,
        n = inout(reg) steps => _,
        inout("d28") k0 => _, inout("d29") k1 => _,
        inout("d30") mask => _, inout("d31") mask => _);
    }
}

/// See [`crate::isp::scalar::demosaic_edge_row`], in 16-bit lanes as the x86 kernel carries
/// them: the opposite colour plus 24576 (`0x6000`) fits unsigned 16 bits, `vhadd` is the floor
/// mean, `vrshr #2` adds the 2, and `vqsub` of 6144 (`0x1800`) is the clamp at 0.
pub(super) unsafe fn demosaic_edge(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    steps: usize,
    kind: RowKind,
) {
    let [u2, u1, c0, d1, d2] = rows;
    let [r, g, b] = out;
    let (xo, yo) = xy(r, b, kind);
    let mask = green_mask(kind);
    // SAFETY: per the module; each step reads columns -2..=9 of its block (the outer rows only
    // column 0..=7, the inner ones -1..=8).
    unsafe {
        neon!([], [
            // The centre row: w2 (q0), w1 (q1), c (q2), e1 (q3), e2 (q4).
            "vld1.16 {{d0, d1}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d2, d3}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d4, d5}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d6, d7}}, [{c0}]",
            "add {c0}, {c0}, #2",
            "vld1.16 {{d8, d9}}, [{c0}]",
            "add {c0}, {c0}, #8",
            // dh (q5), gh4 = 2 (w1 + e1 + c) - w2 - e2 (q6).
            "vabd.u16 q5, q0, q2",
            "vabd.u16 q6, q4, q2",
            "vadd.i16 q5, q5, q6",
            "vabd.u16 q6, q1, q3",
            "vadd.i16 q5, q5, q6",
            "vadd.i16 q0, q0, q4",
            "vadd.i16 q6, q1, q3",
            "vadd.i16 q6, q6, q2",
            "vshl.i16 q6, q6, #1",
            "vsub.i16 q6, q6, q0",
            // The centre column: n2 (q0), s2 (q4), n1 (q7), s1 (q8).
            "vld1.16 {{d0, d1}}, [{u2}]!",
            "vld1.16 {{d8, d9}}, [{d2}]!",
            "add {u1}, {u1}, #2",
            "vld1.16 {{d14, d15}}, [{u1}]",
            "add {d1}, {d1}, #2",
            "vld1.16 {{d16, d17}}, [{d1}]",
            // dv (q9), gv4 (q10).
            "vabd.u16 q9, q0, q2",
            "vabd.u16 q10, q4, q2",
            "vadd.i16 q9, q9, q10",
            "vabd.u16 q10, q7, q8",
            "vadd.i16 q9, q9, q10",
            "vadd.i16 q0, q0, q4",
            "vadd.i16 q10, q7, q8",
            "vadd.i16 q10, q10, q2",
            "vshl.i16 q10, q10, #1",
            "vsub.i16 q10, q10, q0",
            // g4 (q0): the axis with the smaller gradient, the floor mean on a tie.
            "vhadd.s16 q0, q6, q10",
            "vcgt.u16 q11, q5, q9",
            "vbit q0, q10, q11",
            "vcgt.u16 q11, q9, q5",
            "vbit q0, q6, q11",
            // Quarters clamped to 0..=4095 (q12, q11): X, then the green-site Y (q10), green.
            "vmvn.i16 q11, #0xF000",
            "vmov.i16 q12, #0",
            "vrshr.s16 q6, q6, #2",
            "vmax.s16 q6, q6, q12",
            "vmin.s16 q6, q6, q11",
            "vbif q6, q2, q15",
            "vst1.16 {{d12, d13}}, [{x}]!",
            "vrshr.s16 q10, q10, #2",
            "vmax.s16 q10, q10, q12",
            "vmin.s16 q10, q10, q11",
            "vrshr.s16 q4, q0, #2",
            "vmax.s16 q4, q4, q12",
            "vmin.s16 q4, q4, q11",
            "vbit q4, q2, q15",
            "vst1.16 {{d8, d9}}, [{g}]!",
            // base = g4 + 24576 - (w1 + e1 + n1 + s1) (q0).
            "vadd.i16 q2, q1, q3",
            "vadd.i16 q4, q7, q8",
            "vadd.i16 q2, q2, q4",
            "vmov.i16 q4, #0x6000",
            "vadd.i16 q0, q0, q4",
            "vsub.i16 q0, q0, q2",
            // |w1 + n1 - e1 - s1| (q5), |e1 + n1 - w1 - s1| (q6).
            "vadd.i16 q2, q1, q7",
            "vadd.i16 q4, q3, q8",
            "vabd.u16 q5, q2, q4",
            "vadd.i16 q2, q3, q7",
            "vadd.i16 q4, q1, q8",
            "vabd.u16 q6, q2, q4",
            // The diagonals: nw1 (q1), ne1 (q3), sw1 (q7), se1 (q8).
            "sub {u1}, {u1}, #2",
            "vld1.16 {{d2, d3}}, [{u1}]",
            "add {u1}, {u1}, #4",
            "vld1.16 {{d6, d7}}, [{u1}]",
            "add {u1}, {u1}, #12",
            "sub {d1}, {d1}, #2",
            "vld1.16 {{d14, d15}}, [{d1}]",
            "add {d1}, {d1}, #4",
            "vld1.16 {{d16, d17}}, [{d1}]",
            "add {d1}, {d1}, #12",
            // d_1 (q5), d_2 (q6), y1 (q1), y2 (q3).
            "vabd.u16 q2, q1, q8",
            "vshl.i16 q2, q2, #1",
            "vadd.i16 q5, q5, q2",
            "vabd.u16 q2, q3, q7",
            "vshl.i16 q2, q2, #1",
            "vadd.i16 q6, q6, q2",
            "vadd.i16 q1, q1, q8",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q1, q1, q0",
            "vadd.i16 q3, q3, q7",
            "vshl.i16 q3, q3, #1",
            "vadd.i16 q3, q3, q0",
            // y4 (q0), its quarter less the offset, then Y.
            "vhadd.u16 q0, q1, q3",
            "vcgt.u16 q2, q5, q6",
            "vbit q0, q3, q2",
            "vcgt.u16 q2, q6, q5",
            "vbit q0, q1, q2",
            "vrshr.u16 q0, q0, #2",
            "vmov.i16 q2, #0x1800",
            "vqsub.u16 q0, q0, q2",
            "vmin.u16 q0, q0, q11",
            "vbit q0, q10, q15",
            "vst1.16 {{d0, d1}}, [{y}]!",
        ], [],
        u2 = inout(reg) u2.as_ptr().add(2) => _,
        u1 = inout(reg) u1.as_ptr().add(1) => _,
        c0 = inout(reg) c0.as_ptr() => _,
        d1 = inout(reg) d1.as_ptr().add(1) => _,
        d2 = inout(reg) d2.as_ptr().add(2) => _,
        g = inout(reg) g.as_mut_ptr() => _,
        x = inout(reg) xo.as_mut_ptr() => _,
        y = inout(reg) yo.as_mut_ptr() => _,
        n = inout(reg) steps => _,
        inout("d30") mask => _, inout("d31") mask => _);
    }
}

pub(super) unsafe fn bayer_luma(rows: [&[u16]; 3], dst: &mut [u16], steps: usize) {
    let [up, mid, dn] = rows;
    // SAFETY: per the module; as `demosaic_bilinear`.
    unsafe {
        neon!([], [
            // Column sums up + 2 mid + dn at -1 (q3), 0 (q4), 1 (q5).
            "vld1.16 {{d0, d1}}, [{up}]",
            "add {up}, {up}, #2",
            "vld1.16 {{d2, d3}}, [{mid}]",
            "add {mid}, {mid}, #2",
            "vld1.16 {{d4, d5}}, [{dn}]",
            "add {dn}, {dn}, #2",
            "vadd.i16 q0, q0, q2",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q3, q0, q1",
            "vld1.16 {{d0, d1}}, [{up}]",
            "add {up}, {up}, #2",
            "vld1.16 {{d2, d3}}, [{mid}]",
            "add {mid}, {mid}, #2",
            "vld1.16 {{d4, d5}}, [{dn}]",
            "add {dn}, {dn}, #2",
            "vadd.i16 q0, q0, q2",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q4, q0, q1",
            "vld1.16 {{d0, d1}}, [{up}]",
            "add {up}, {up}, #12",
            "vld1.16 {{d2, d3}}, [{mid}]",
            "add {mid}, {mid}, #12",
            "vld1.16 {{d4, d5}}, [{dn}]",
            "add {dn}, {dn}, #12",
            "vadd.i16 q0, q0, q2",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q5, q0, q1",
            "vadd.i16 q3, q3, q5",
            "vshl.i16 q4, q4, #1",
            "vadd.i16 q3, q3, q4",
            "vrshr.u16 q3, q3, #4",
            "vst1.16 {{d6, d7}}, [{d}]!",
        ], [],
        up = inout(reg) up.as_ptr() => _,
        mid = inout(reg) mid.as_ptr() => _,
        dn = inout(reg) dn.as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

pub(super) unsafe fn quad_rgb(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    steps: usize,
    pattern: CfaOrder,
) {
    let [r, g, b] = out;
    // Greens on the anti-diagonal (top odd, bottom even) or the diagonal; `p` takes the other
    // top sample and `q` the other bottom one.
    let (anti, p, q) = match pattern {
        CfaOrder::Rggb => (true, r, b),
        CfaOrder::Bggr => (true, b, r),
        CfaOrder::Grbg => (false, r, b),
        CfaOrder::Gbrg => (false, b, r),
    };
    macro_rules! quad {
        ($green:literal, $p:literal, $q:literal) => {
            neon!([], [
                "vld2.16 {{d0, d1, d2, d3}}, [{t}]!",
                "vld2.16 {{d4, d5, d6, d7}}, [{b}]!",
                $green,
                $p,
                "vst1.16 {{d8, d9}}, [{g}]!",
                $q,
            ], [],
            t = inout(reg) top.as_ptr() => _,
            b = inout(reg) bottom.as_ptr() => _,
            p = inout(reg) p.as_mut_ptr() => _,
            g = inout(reg) g.as_mut_ptr() => _,
            q = inout(reg) q.as_mut_ptr() => _,
            n = inout(reg) steps => _)
        };
    }
    // SAFETY: per the module; 16 samples of each input row a step.
    unsafe {
        if anti {
            quad!(
                "vrhadd.u16 q4, q1, q2",
                "vst1.16 {{d0, d1}}, [{p}]!",
                "vst1.16 {{d6, d7}}, [{q}]!"
            );
        } else {
            quad!(
                "vrhadd.u16 q4, q0, q3",
                "vst1.16 {{d2, d3}}, [{p}]!",
                "vst1.16 {{d4, d5}}, [{q}]!"
            );
        }
    }
}

pub(super) unsafe fn quad_luma(top: &[u16], bottom: &[u16], dst: &mut [u16], steps: usize) {
    // SAFETY: per the module; as `quad_rgb`.
    unsafe {
        neon!([], [
            "vld2.16 {{d0, d1, d2, d3}}, [{t}]!",
            "vld2.16 {{d4, d5, d6, d7}}, [{b}]!",
            "vadd.i16 q0, q0, q1",
            "vadd.i16 q2, q2, q3",
            "vadd.i16 q0, q0, q2",
            "vrshr.u16 q0, q0, #2",
            "vst1.16 {{d0, d1}}, [{d}]!",
        ], [],
        t = inout(reg) top.as_ptr() => _,
        b = inout(reg) bottom.as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// Returns the sums of the `steps` blocks, as [`crate::isp::scalar::zone_sums`].
pub(super) unsafe fn zone_sums(rgb: [&[u16]; 3], steps: usize, sat: u16) -> [u32; 5] {
    let mut sums = [0u32; 5];
    // SAFETY: per the module; the five sums are stored to `sums`.
    unsafe {
        neon!([
            "vdup.16 q13, {sat}",
            "vmov.i32 q8, #0",
            "vmov.i32 q9, #0",
            "vmov.i32 q10, #0",
            "vmov.i32 q11, #0",
            "vmov.i32 q12, #0",
        ], [
            "vld1.16 {{d0, d1}}, [{r}]!",
            "vld1.16 {{d2, d3}}, [{g}]!",
            "vld1.16 {{d4, d5}}, [{b}]!",
            "vmax.u16 q3, q0, q1",
            "vmax.u16 q3, q3, q2",
            "vcgt.u16 q3, q13, q3",
            "vand q4, q0, q3",
            "vpadal.u16 q8, q4",
            "vand q4, q1, q3",
            "vpadal.u16 q9, q4",
            "vand q4, q2, q3",
            "vpadal.u16 q10, q4",
            "vshr.u16 q3, q3, #15",
            "vpadal.u16 q11, q3",
            "vadd.i16 q0, q0, q2",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q0, q0, q1",
            "vrshr.u16 q0, q0, #2",
            "vpadal.u16 q12, q0",
        ], [
            "vpadd.u32 d16, d16, d17",
            "vpadd.u32 d18, d18, d19",
            "vpadd.u32 d20, d20, d21",
            "vpadd.u32 d22, d22, d23",
            "vpadd.u32 d24, d24, d25",
            "vpadd.u32 d16, d16, d18",
            "vpadd.u32 d17, d20, d22",
            "vpadd.u32 d18, d24, d24",
            "vst1.32 {{d16, d17}}, [{o}]!",
            "vst1.32 {{d18[0]}}, [{o}]",
        ],
        sat = in(reg) u32::from(sat),
        r = inout(reg) rgb[0].as_ptr() => _,
        g = inout(reg) rgb[1].as_ptr() => _,
        b = inout(reg) rgb[2].as_ptr() => _,
        o = inout(reg) sums.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
    sums
}

pub(super) unsafe fn luma_bins(rgb: [&[u16]; 3], dst: &mut [u16], bins: u16, steps: usize) {
    // SAFETY: per the module.
    unsafe {
        neon!([
            "vdup.16 q13, {bins}",
        ], [
            "vld1.16 {{d0, d1}}, [{r}]!",
            "vld1.16 {{d2, d3}}, [{g}]!",
            "vld1.16 {{d4, d5}}, [{b}]!",
            "vadd.i16 q0, q0, q2",
            "vshl.i16 q1, q1, #1",
            "vadd.i16 q0, q0, q1",
            "vrshr.u16 q0, q0, #2",
            "vmull.u16 q4, d0, d26",
            "vmull.u16 q5, d1, d27",
            "vshrn.u32 d0, q4, #12",
            "vshrn.u32 d1, q5, #12",
            "vst1.16 {{d0, d1}}, [{d}]!",
        ], [],
        bins = in(reg) u32::from(bins),
        r = inout(reg) rgb[0].as_ptr() => _,
        g = inout(reg) rgb[1].as_ptr() => _,
        b = inout(reg) rgb[2].as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

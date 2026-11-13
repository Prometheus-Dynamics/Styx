//! Colour-domain kernels in 32-bit Arm NEON assembly: matrix, narrowing, tone curves, RGB
//! interleave, YCbCr. Each runs `steps` blocks (of 8 or 16 pixels, as the wrapper says).
//!
//! # Safety (every function)
//! The CPU must have NEON, and the slices must hold `steps` blocks of input and output.

use super::{lanes16, neon};
use crate::simd::YuvCoeffs;

/// 8 pixels a step.
pub(super) unsafe fn ccm(planes: [&mut [u16]; 3], m: &[i16; 9], steps: usize) {
    let [r, g, b] = planes;
    // Three whole registers for the by-lane multiplies.
    let mut k = [0i16; 12];
    k[..9].copy_from_slice(m);
    // SAFETY: per the module; `k` is 24 bytes.
    unsafe {
        neon!([
            "vld1.16 {{d0, d1, d2}}, [{m}]",
            "vmov.i16 q2, #0",
            "vmvn.i16 q3, #0xF000",
        ], [
            "vld1.16 {{d16, d17}}, [{r}]",
            "vld1.16 {{d18, d19}}, [{g}]",
            "vld1.16 {{d20, d21}}, [{b}]",
            "vshl.i16 q8, q8, #3",
            "vshl.i16 q9, q9, #3",
            "vshl.i16 q10, q10, #3",
            "vqrdmulh.s16 q4, q8, d0[0]",
            "vqrdmulh.s16 q12, q9, d0[1]",
            "vqadd.s16 q4, q4, q12",
            "vqrdmulh.s16 q12, q10, d0[2]",
            "vqadd.s16 q4, q4, q12",
            "vmax.s16 q4, q4, q2",
            "vmin.s16 q4, q4, q3",
            "vqrdmulh.s16 q5, q8, d0[3]",
            "vqrdmulh.s16 q12, q9, d1[0]",
            "vqadd.s16 q5, q5, q12",
            "vqrdmulh.s16 q12, q10, d1[1]",
            "vqadd.s16 q5, q5, q12",
            "vmax.s16 q5, q5, q2",
            "vmin.s16 q5, q5, q3",
            "vqrdmulh.s16 q6, q8, d1[2]",
            "vqrdmulh.s16 q12, q9, d1[3]",
            "vqadd.s16 q6, q6, q12",
            "vqrdmulh.s16 q12, q10, d2[0]",
            "vqadd.s16 q6, q6, q12",
            "vmax.s16 q6, q6, q2",
            "vmin.s16 q6, q6, q3",
            "vst1.16 {{d8, d9}}, [{r}]!",
            "vst1.16 {{d10, d11}}, [{g}]!",
            "vst1.16 {{d12, d13}}, [{b}]!",
        ], [],
        m = in(reg) k.as_ptr(),
        r = inout(reg) r.as_mut_ptr() => _,
        g = inout(reg) g.as_mut_ptr() => _,
        b = inout(reg) b.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// 16 pixels a step.
pub(super) unsafe fn narrow(src: &[u16], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        neon!([], [
            "vld1.16 {{d0, d1, d2, d3}}, [{s}]!",
            "vqshrn.u16 d4, q0, #4",
            "vqshrn.u16 d5, q1, #4",
            "vst1.8 {{d4, d5}}, [{d}]!",
        ], [],
        s = inout(reg) src.as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// 8 pixels a step: clamped in vectors, looked up one by one.
pub(super) unsafe fn lut(src: &[u16], dst: &mut [u8], lut: &[u8; 4096], steps: usize) {
    macro_rules! lane {
        ($lane:literal) => {
            concat!(
                "vmov.u16 {t}, ",
                $lane,
                "\n",
                "ldrb {t}, [{lut}, {t}]\n",
                "strb {t}, [{d}], #1"
            )
        };
    }
    // SAFETY: per the module; every index is clamped to 4095.
    unsafe {
        neon!([
            "vmvn.i16 q8, #0xF000",
        ], [
            "vld1.16 {{d0, d1}}, [{s}]!",
            "vmin.u16 q0, q0, q8",
            lane!("d0[0]"),
            lane!("d0[1]"),
            lane!("d0[2]"),
            lane!("d0[3]"),
            lane!("d1[0]"),
            lane!("d1[1]"),
            lane!("d1[2]"),
            lane!("d1[3]"),
        ], [],
        lut = in(reg) lut.as_ptr(),
        t = out(reg) _,
        s = inout(reg) src.as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// [`crate::simd::poly::PolyTone`], 8 pixels a step: the octave from the leading zeros, the
/// position by a per-lane shift, the coefficients with `vtbl` (four registers per table).
pub(super) unsafe fn poly(src: &[u16], dst: &mut [u8], c: &[[[i16; 8]; 2]; 3], steps: usize) {
    // SAFETY: per the module; the tables are 96 bytes.
    unsafe {
        neon!([
            "vld1.8 {{d16, d17, d18, d19}}, [{c}]!",
            "vld1.8 {{d20, d21, d22, d23}}, [{c}]!",
            "vld1.8 {{d24, d25, d26, d27}}, [{c}]",
            "vmvn.i16 q7, #0xF000",
        ], [
            "vld1.16 {{d0, d1}}, [{s}]!",
            "vmin.u16 q0, q0, q7",
            "vmov.i16 q1, #32",
            "vadd.i16 q0, q0, q1",
            // 15 - floor(log2 v), at least 4: the top octave takes 4096 and up.
            "vclz.i16 q2, q0",
            "vmov.i16 q1, #4",
            "vmax.u16 q2, q2, q1",
            // t = v << (lz - 1) - 2^14.
            "vmov.i16 q1, #1",
            "vsub.i16 q3, q2, q1",
            "vshl.u16 q3, q0, q3",
            "vmov.i16 q1, #0x4000",
            "vsub.i16 q3, q3, q1",
            // Index bytes 16 half + 2 octave (+ 1): half min(t >> 13, 1), octave 10 - lz.
            "vshr.u16 q4, q3, #13",
            "vmov.i16 q1, #1",
            "vmin.u16 q4, q4, q1",
            "vshl.i16 q4, q4, #4",
            "vmov.i16 q1, #20",
            "vshl.i16 q2, q2, #1",
            "vsub.i16 q1, q1, q2",
            "vadd.i16 q1, q1, q4",
            "vmov.i16 q2, #1",
            "vadd.i16 q2, q1, q2",
            "vsli.16 q1, q2, #8",
            "vtbl.8 d4, {{d16, d17, d18, d19}}, d2",
            "vtbl.8 d5, {{d16, d17, d18, d19}}, d3",
            "vtbl.8 d8, {{d20, d21, d22, d23}}, d2",
            "vtbl.8 d9, {{d20, d21, d22, d23}}, d3",
            "vtbl.8 d10, {{d24, d25, d26, d27}}, d2",
            "vtbl.8 d11, {{d24, d25, d26, d27}}, d3",
            "vqrdmulh.s16 q5, q5, q3",
            "vqadd.s16 q4, q4, q5",
            "vqrdmulh.s16 q4, q4, q3",
            "vqadd.s16 q2, q2, q4",
            // mulhrs(y, 1024) is (y + 16) >> 5: a rounding narrowing shift.
            "vqrshrun.s16 d0, q2, #5",
            "vst1.8 {{d0}}, [{d}]!",
        ], [],
        c = inout(reg) c.as_ptr() => _,
        s = inout(reg) src.as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// 16 pixels a step.
pub(super) unsafe fn interleave_rgb(planes: [&[u8]; 3], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        neon!([], [
            "vld1.8 {{d0, d1}}, [{r}]!",
            "vld1.8 {{d2, d3}}, [{g}]!",
            "vld1.8 {{d4, d5}}, [{b}]!",
            "vst3.8 {{d0, d2, d4}}, [{d}]!",
            "vst3.8 {{d1, d3, d5}}, [{d}]!",
        ], [],
        r = inout(reg) planes[0].as_ptr() => _,
        g = inout(reg) planes[1].as_ptr() => _,
        b = inout(reg) planes[2].as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _);
    }
}

/// A byte in every lane of a 64-bit register.
fn bytes(v: u8) -> f64 {
    f64::from_bits(u64::from(v) * 0x0101_0101_0101_0101)
}

/// 16 pixels a step.
pub(super) unsafe fn rgb_to_y(planes: [&[u8]; 3], dst: &mut [u8], steps: usize, c: &YuvCoeffs) {
    // SAFETY: per the module.
    unsafe {
        neon!([], [
            "vld1.8 {{d0, d1}}, [{r}]!",
            "vld1.8 {{d2, d3}}, [{g}]!",
            "vld1.8 {{d4, d5}}, [{b}]!",
            "vmull.u8 q4, d0, d28",
            "vmlal.u8 q4, d2, d29",
            "vmlal.u8 q4, d4, d30",
            "vmull.u8 q5, d1, d28",
            "vmlal.u8 q5, d3, d29",
            "vmlal.u8 q5, d5, d30",
            "vrshrn.u16 d0, q4, #8",
            "vrshrn.u16 d1, q5, #8",
            "vqadd.u8 d0, d0, d31",
            "vqadd.u8 d1, d1, d31",
            "vst1.8 {{d0, d1}}, [{d}]!",
        ], [],
        r = inout(reg) planes[0].as_ptr() => _,
        g = inout(reg) planes[1].as_ptr() => _,
        b = inout(reg) planes[2].as_ptr() => _,
        d = inout(reg) dst.as_mut_ptr() => _,
        n = inout(reg) steps => _,
        inout("d28") bytes(c.y[0] as u8) => _, inout("d29") bytes(c.y[1] as u8) => _,
        inout("d30") bytes(c.y[2] as u8) => _, inout("d31") bytes(c.y_offset as u8) => _);
    }
}

/// 8 chroma pairs a step (16 pixels of each input row).
// Many borrowed rows, as the scalar oracle.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn rgb_to_uv(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    steps: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) {
    let row = |k: &[i16; 3]| lanes16([k[0] as u16, k[1] as u16, k[2] as u16, 0]);
    let (ku, kv) = (row(&c.u), row(&c.v));
    macro_rules! uv {
        ([$($store:literal),*], $($v:tt)*) => {
            neon!([
                "vmov d6, d28",
                "vmov d7, d29",
                "vmov.i16 q13, #128",
            ], [
                // Rounded 2x2 means of R (q8), G (q9), B (q10).
                "vld1.8 {{d0, d1}}, [{tr}]!",
                "vpaddl.u8 q8, q0",
                "vld1.8 {{d0, d1}}, [{br}]!",
                "vpadal.u8 q8, q0",
                "vrshr.u16 q8, q8, #2",
                "vld1.8 {{d0, d1}}, [{tg}]!",
                "vpaddl.u8 q9, q0",
                "vld1.8 {{d0, d1}}, [{bg}]!",
                "vpadal.u8 q9, q0",
                "vrshr.u16 q9, q9, #2",
                "vld1.8 {{d0, d1}}, [{tb}]!",
                "vpaddl.u8 q10, q0",
                "vld1.8 {{d0, d1}}, [{bb}]!",
                "vpadal.u8 q10, q0",
                "vrshr.u16 q10, q10, #2",
                "vmul.s16 q11, q8, d6[0]",
                "vmla.s16 q11, q9, d6[1]",
                "vmla.s16 q11, q10, d6[2]",
                "vrshr.s16 q11, q11, #7",
                "vadd.i16 q11, q11, q13",
                "vqmovun.s16 d22, q11",
                "vmul.s16 q12, q8, d7[0]",
                "vmla.s16 q12, q9, d7[1]",
                "vmla.s16 q12, q10, d7[2]",
                "vrshr.s16 q12, q12, #7",
                "vadd.i16 q12, q12, q13",
                "vqmovun.s16 d23, q12",
                $($store),*
            ], [],
            tr = inout(reg) top[0].as_ptr() => _,
            tg = inout(reg) top[1].as_ptr() => _,
            tb = inout(reg) top[2].as_ptr() => _,
            br = inout(reg) bottom[0].as_ptr() => _,
            bg = inout(reg) bottom[1].as_ptr() => _,
            bb = inout(reg) bottom[2].as_ptr() => _,
            u = inout(reg) u.as_mut_ptr() => _,
            $($v)*
            n = inout(reg) steps => _,
            inout("d28") ku => _, inout("d29") kv => _)
        };
    }
    // SAFETY: per the module.
    unsafe {
        if interleaved {
            uv!(["vst2.8 {{d22, d23}}, [{u}]!"],);
        } else {
            uv!(
                ["vst1.8 {{d22}}, [{u}]!", "vst1.8 {{d23}}, [{v}]!"],
                v = inout(reg) v.as_mut_ptr() => _,
            );
        }
    }
}

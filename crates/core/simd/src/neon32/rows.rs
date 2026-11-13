//! Row leaves in 32-bit Arm NEON assembly. Each runs `steps` blocks of its block size and
//! returns nothing; the caller checks the CPU, sizes the rows and finishes the tail.
//!
//! # Safety (every function)
//! The CPU must have NEON, and the slices must hold `steps` blocks of input and output.

use core::arch::asm;

use crate::{ColorLayout, YuvMatrix};

/// The loop around one block: `body` runs with `{s}`, `{d}` advancing and `{n}` counting down.
macro_rules! block_loop {
    ($s:expr, $d:expr, $n:expr, [$($body:literal),* $(,)?] $(, $($extra:tt)*)?) => {
        asm!(
            ".fpu neon",
            "2:",
            $($body,)*
            "subs {n}, {n}, #1",
            "bne 2b",
            s = inout(reg) $s => _,
            d = inout(reg) $d => _,
            n = inout(reg) $n => _,
            $($($extra)*,)?
            options(nostack),
        )
    };
}

/// 16 pixels a step.
pub(super) unsafe fn swap_rb24(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld3.8 {{d0, d2, d4}}, [{s}]!",
            "vld3.8 {{d1, d3, d5}}, [{s}]!",
            "vswp q0, q2",
            "vst3.8 {{d0, d2, d4}}, [{d}]!",
            "vst3.8 {{d1, d3, d5}}, [{d}]!",
        ], out("q0") _, out("q1") _, out("q2") _);
    }
}

/// 16 pixels a step.
pub(super) unsafe fn swap_rb32(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld4.8 {{d0, d2, d4, d6}}, [{s}]!",
            "vld4.8 {{d1, d3, d5, d7}}, [{s}]!",
            "vswp q0, q2",
            "vst4.8 {{d0, d2, d4, d6}}, [{d}]!",
            "vst4.8 {{d1, d3, d5, d7}}, [{d}]!",
        ], out("q0") _, out("q1") _, out("q2") _, out("q3") _);
    }
}

/// 16 pixels a step.
pub(super) unsafe fn x32_to_rgb24(src: &[u8], dst: &mut [u8], steps: usize, swap: bool) {
    // SAFETY: per the module.
    unsafe {
        if swap {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld4.8 {{d0, d2, d4, d6}}, [{s}]!",
                "vld4.8 {{d1, d3, d5, d7}}, [{s}]!",
                "vswp q0, q2",
                "vst3.8 {{d0, d2, d4}}, [{d}]!",
                "vst3.8 {{d1, d3, d5}}, [{d}]!",
            ], out("q0") _, out("q1") _, out("q2") _, out("q3") _);
        } else {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld4.8 {{d0, d2, d4, d6}}, [{s}]!",
                "vld4.8 {{d1, d3, d5, d7}}, [{s}]!",
                "vst3.8 {{d0, d2, d4}}, [{d}]!",
                "vst3.8 {{d1, d3, d5}}, [{d}]!",
            ], out("q0") _, out("q1") _, out("q2") _, out("q3") _);
        }
    }
}

/// 8 pixels a step: `(77 R + 150 G + 29 B) >> 8`, the coefficients put on bytes 0, 1, 2 in
/// the layout's order (the sum is at most 65280, so 16 bits hold it).
pub(super) unsafe fn rgb_to_luma(src: &[u8], dst: &mut [u8], steps: usize, layout: ColorLayout) {
    let mut c = [0u32; 3];
    for (o, k) in layout.rgb_offsets().into_iter().zip([77, 150, 29]) {
        c[o] = k;
    }
    // SAFETY: per the module.
    unsafe {
        if layout.bytes_per_pixel() == 3 {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld3.8 {{d0, d1, d2}}, [{s}]!",
                "vmull.u8 q8, d0, d28",
                "vmlal.u8 q8, d1, d29",
                "vmlal.u8 q8, d2, d30",
                "vshrn.u16 d0, q8, #8",
                "vst1.8 {{d0}}, [{d}]!",
            ], inout("d28") f64::from_bits(u64::from(c[0]) * 0x0101_0101_0101_0101) => _,
               inout("d29") f64::from_bits(u64::from(c[1]) * 0x0101_0101_0101_0101) => _,
               inout("d30") f64::from_bits(u64::from(c[2]) * 0x0101_0101_0101_0101) => _,
               out("q0") _, out("q1") _, out("q8") _);
        } else {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld4.8 {{d0, d1, d2, d3}}, [{s}]!",
                "vmull.u8 q8, d0, d28",
                "vmlal.u8 q8, d1, d29",
                "vmlal.u8 q8, d2, d30",
                "vshrn.u16 d0, q8, #8",
                "vst1.8 {{d0}}, [{d}]!",
            ], inout("d28") f64::from_bits(u64::from(c[0]) * 0x0101_0101_0101_0101) => _,
               inout("d29") f64::from_bits(u64::from(c[1]) * 0x0101_0101_0101_0101) => _,
               inout("d30") f64::from_bits(u64::from(c[2]) * 0x0101_0101_0101_0101) => _,
               out("q0") _, out("q1") _, out("q8") _);
        }
    }
}

/// 16 pixels a step.
pub(super) unsafe fn rgb24_to_rgba(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vmov.i8 q3, #255",
            "vld3.8 {{d0, d2, d4}}, [{s}]!",
            "vld3.8 {{d1, d3, d5}}, [{s}]!",
            "vst4.8 {{d0, d2, d4, d6}}, [{d}]!",
            "vst4.8 {{d1, d3, d5, d7}}, [{d}]!",
        ], out("q0") _, out("q1") _, out("q2") _, out("q3") _);
    }
}

/// 16 pixels a step.
pub(super) unsafe fn gray8_to_rgb24(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld1.8 {{d0, d1}}, [{s}]!",
            "vmov q1, q0",
            "vmov q2, q0",
            "vst3.8 {{d0, d2, d4}}, [{d}]!",
            "vst3.8 {{d1, d3, d5}}, [{d}]!",
        ], out("q0") _, out("q1") _, out("q2") _);
    }
}

/// 16 pixels a step: `vld2` puts the high bytes in q1.
pub(super) unsafe fn gray16le_to_rgb24(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld2.8 {{d0, d1, d2, d3}}, [{s}]!",
            "vmov q0, q1",
            "vmov q2, q1",
            "vst3.8 {{d0, d2, d4}}, [{d}]!",
            "vst3.8 {{d1, d3, d5}}, [{d}]!",
        ], out("q0") _, out("q1") _, out("q2") _);
    }
}

/// 8 pixels a step: the channels' high bytes by a narrowing shift.
pub(super) unsafe fn rgb48le_to_rgb24(src: &[u8], dst: &mut [u8], steps: usize, swap: bool) {
    // SAFETY: per the module.
    unsafe {
        if swap {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld3.16 {{d0, d2, d4}}, [{s}]!",
                "vld3.16 {{d1, d3, d5}}, [{s}]!",
                "vshrn.u16 d16, q2, #8",
                "vshrn.u16 d17, q1, #8",
                "vshrn.u16 d18, q0, #8",
                "vst3.8 {{d16, d17, d18}}, [{d}]!",
            ], out("q0") _, out("q1") _, out("q2") _, out("q8") _, out("q9") _);
        } else {
            block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
                "vld3.16 {{d0, d2, d4}}, [{s}]!",
                "vld3.16 {{d1, d3, d5}}, [{s}]!",
                "vshrn.u16 d16, q0, #8",
                "vshrn.u16 d17, q1, #8",
                "vshrn.u16 d18, q2, #8",
                "vst3.8 {{d16, d17, d18}}, [{d}]!",
            ], out("q0") _, out("q1") _, out("q2") _, out("q8") _, out("q9") _);
        }
    }
}

/// 16 pixels a step: the even bytes.
pub(super) unsafe fn yuyv_luma(src: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(src.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld2.8 {{d0, d1, d2, d3}}, [{s}]!",
            "vst1.8 {{d0, d1}}, [{d}]!",
        ], out("q0") _, out("q1") _);
    }
}

/// 16 outputs a step: `vld2` splits even and odd bytes and `vrhadd` averages the odd ones,
/// (x + y + 1) >> 1.
pub(super) unsafe fn yuyv_chroma(a: &[u8], b: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(a.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld2.8 {{d0, d1, d2, d3}}, [{s}]!",
            "vld2.8 {{d4, d5, d6, d7}}, [{b}]!",
            "vrhadd.u8 q1, q1, q3",
            "vst1.8 {{d2, d3}}, [{d}]!",
        ], b = inout(reg) b.as_ptr() => _,
           out("q0") _, out("q1") _, out("q2") _, out("q3") _);
    }
}

/// 16 outputs a step: `vld2` splits even and odd bytes, the four bytes sum in 16 bits (at most
/// 1020) and `vrshrn #2` adds 2 before the shift, as the scalar kernel rounds.
pub(super) unsafe fn box2(top: &[u8], bottom: &[u8], dst: &mut [u8], steps: usize) {
    // SAFETY: per the module.
    unsafe {
        block_loop!(top.as_ptr(), dst.as_mut_ptr(), steps, [
            "vld2.8 {{d0, d1, d2, d3}}, [{s}]!",
            "vld2.8 {{d4, d5, d6, d7}}, [{b}]!",
            "vaddl.u8 q8, d0, d2",
            "vaddl.u8 q9, d1, d3",
            "vaddl.u8 q10, d4, d6",
            "vaddl.u8 q11, d5, d7",
            "vadd.u16 q8, q8, q10",
            "vadd.u16 q9, q9, q11",
            "vrshrn.u16 d0, q8, #2",
            "vrshrn.u16 d1, q9, #2",
            "vst1.8 {{d0, d1}}, [{d}]!",
        ], b = inout(reg) bottom.as_ptr() => _,
           out("q0") _, out("q1") _, out("q2") _, out("q3") _,
           out("q8") _, out("q9") _, out("q10") _, out("q11") _);
    }
}

/// Reverse 16 bytes a step of 1-, 2- or 4-byte pixels (`bpp`) or 8 pixels of 3 bytes, the
/// source read forwards from its start and the output written backwards from `dst_end`, the
/// last block's start.
pub(super) unsafe fn reverse(src: &[u8], dst_end: *mut u8, steps: usize, bpp: usize) {
    // SAFETY: per the module; `dst_end` is the start of the last of `steps` output blocks.
    unsafe {
        match bpp {
            1 => block_loop!(src.as_ptr(), dst_end, steps, [
                "vld1.8 {{d0, d1}}, [{s}]!",
                "vrev64.8 q0, q0",
                "vswp d0, d1",
                "vst1.8 {{d0, d1}}, [{d}], {back}",
            ], back = in(reg) -16isize, out("q0") _),
            2 => block_loop!(src.as_ptr(), dst_end, steps, [
                "vld1.8 {{d0, d1}}, [{s}]!",
                "vrev64.16 q0, q0",
                "vswp d0, d1",
                "vst1.8 {{d0, d1}}, [{d}], {back}",
            ], back = in(reg) -16isize, out("q0") _),
            3 => block_loop!(src.as_ptr(), dst_end, steps, [
                "vld3.8 {{d0, d1, d2}}, [{s}]!",
                "vrev64.8 d0, d0",
                "vrev64.8 d1, d1",
                "vrev64.8 d2, d2",
                "vst3.8 {{d0, d1, d2}}, [{d}], {back}",
            ], back = in(reg) -24isize, out("q0") _, out("q1") _),
            _ => block_loop!(src.as_ptr(), dst_end, steps, [
                "vld1.8 {{d0, d1}}, [{s}]!",
                "vrev64.32 q0, q0",
                "vswp d0, d1",
                "vst1.8 {{d0, d1}}, [{d}], {back}",
            ], back = in(reg) -16isize, out("q0") _),
        }
    }
}

/// YUV → RGB, 16 pixels a step, from U in d0, V in d1 and the even and odd lumas in d2 and
/// d3 (each block's `$load`). Chroma terms and luma widen to 32 bits as the scalar kernel
/// computes (`y_scale·c` reaches 75990), from the coefficients in d7, the luma scale in
/// d30 and its offset in d31; `vqshrun #8` then `vqmovn` shift and clamp. Even and
/// odd results zip back into pixel order; `vst3` writes 48 bytes.
macro_rules! yuv_rgb_loop {
    ($s:expr, $d:expr, $n:expr, $m:expr, [$($load:literal),* $(,)?] $(, $($extra:tt)*)?) => {{
        let m: &YuvMatrix = $m;
        let (sc, off) = (m.y_scale, u16::from(m.y_offset) * 0x101);
        let k: [u16; 12] = [m.r_v, m.g_u, m.g_v, m.b_u, sc, sc, sc, sc, off, off, off, off];
        asm!(
            ".fpu neon",
            "vld1.16 {{d7}}, [{k}]!",
            "vld1.16 {{d30, d31}}, [{k}]",
            "vmov.i8 d6, #128",
            "2:",
            $($load,)*
            "vsubl.u8 q8, d0, d6",
            "vsubl.u8 q9, d1, d6",
            "vqsub.u8 d2, d2, d31",
            "vqsub.u8 d3, d3, d31",
            "vmovl.u8 q10, d2",
            "vmovl.u8 q11, d3",
            "vmov.i32 q4, #128",
            "vmov.i32 q5, #128",
            "vmov.i32 q6, #128",
            "vmov.i32 q7, #128",
            "vmlal.u16 q4, d20, d30",
            "vmlal.u16 q5, d21, d30",
            "vmlal.u16 q6, d22, d30",
            "vmlal.u16 q7, d23, d30",
            // Red: + r_v·e.
            "vmull.s16 q10, d18, d7[0]",
            "vmull.s16 q11, d19, d7[0]",
            "vadd.i32 q12, q4, q10",
            "vadd.i32 q13, q5, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d0, q14",
            "vadd.i32 q12, q6, q10",
            "vadd.i32 q13, q7, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d1, q14",
            // Green: - g_u·d - g_v·e.
            "vmull.s16 q10, d16, d7[1]",
            "vmlal.s16 q10, d18, d7[2]",
            "vmull.s16 q11, d17, d7[1]",
            "vmlal.s16 q11, d19, d7[2]",
            "vsub.i32 q12, q4, q10",
            "vsub.i32 q13, q5, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d2, q14",
            "vsub.i32 q12, q6, q10",
            "vsub.i32 q13, q7, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d3, q14",
            // Blue: + b_u·d.
            "vmull.s16 q10, d16, d7[3]",
            "vmull.s16 q11, d17, d7[3]",
            "vadd.i32 q12, q4, q10",
            "vadd.i32 q13, q5, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d4, q14",
            "vadd.i32 q12, q6, q10",
            "vadd.i32 q13, q7, q11",
            "vqshrun.s32 d28, q12, #8",
            "vqshrun.s32 d29, q13, #8",
            "vqmovn.u16 d5, q14",
            "vzip.8 d0, d1",
            "vzip.8 d2, d3",
            "vzip.8 d4, d5",
            "vst3.8 {{d0, d2, d4}}, [{d}]!",
            "vst3.8 {{d1, d3, d5}}, [{d}]!",
            "subs {n}, {n}, #1",
            "bne 2b",
            s = inout(reg) $s => _,
            d = inout(reg) $d => _,
            n = inout(reg) $n => _,
            k = inout(reg) k.as_ptr() => _,
            $($($extra)*,)?
            out("q0") _, out("q1") _, out("q2") _, out("q3") _, out("q4") _, out("q5") _,
            out("q6") _, out("q7") _, out("q8") _, out("q9") _, out("q10") _, out("q11") _,
            out("q12") _, out("q13") _, out("q14") _, out("q15") _,
            options(nostack),
        )
    }};
}

/// NV12 → RGB24, 16 pixels a step.
pub(super) unsafe fn nv12_rgb(y: &[u8], uv: &[u8], dst: &mut [u8], steps: usize, m: &YuvMatrix) {
    // SAFETY: per the module; 16 chroma bytes a step, as many as luma.
    unsafe {
        yuv_rgb_loop!(y.as_ptr(), dst.as_mut_ptr(), steps, m, [
            "vld2.8 {{d0, d1}}, [{c}]!",
            "vld2.8 {{d2, d3}}, [{s}]!",
        ], c = inout(reg) uv.as_ptr() => _);
    }
}

/// I420 → RGB24, 16 pixels a step.
pub(super) unsafe fn i420_rgb(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    dst: &mut [u8],
    steps: usize,
    m: &YuvMatrix,
) {
    // SAFETY: per the module; 8 bytes of each chroma a step.
    unsafe {
        yuv_rgb_loop!(y.as_ptr(), dst.as_mut_ptr(), steps, m, [
            "vld1.8 {{d0}}, [{u}]!",
            "vld1.8 {{d1}}, [{v}]!",
            "vld2.8 {{d2, d3}}, [{s}]!",
        ], u = inout(reg) u.as_ptr() => _, v = inout(reg) v.as_ptr() => _);
    }
}

/// YUYV → RGB24, 16 pixels (32 source bytes) a step: `vld4` splits Y0 U Y1 V.
pub(super) unsafe fn yuyv_rgb(src: &[u8], dst: &mut [u8], steps: usize, m: &YuvMatrix) {
    // SAFETY: per the module.
    unsafe {
        yuv_rgb_loop!(
            src.as_ptr(),
            dst.as_mut_ptr(),
            steps,
            m,
            [
                "vld4.8 {{d16, d17, d18, d19}}, [{s}]!",
                "vmov d0, d17",
                "vmov d1, d19",
                "vmov d2, d16",
                "vmov d3, d18",
            ]
        );
    }
}

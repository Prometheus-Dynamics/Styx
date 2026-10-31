//! Row leaves in 32-bit Arm NEON assembly. Each runs `steps` blocks of its block size and
//! returns nothing; the caller checks the CPU, sizes the rows and finishes the tail.
//!
//! # Safety (every function)
//! The CPU must have NEON, and the slices must hold `steps` blocks of input and output.

use core::arch::asm;

use crate::simd::ColorLayout;

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

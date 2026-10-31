//! 32-bit Arm NEON kernels (ARMv7-A, e.g. the RV1106's Cortex-A7), for CPUs whose kernel
//! reports NEON (`AT_HWCAP`). Inline assembly that enables NEON itself (Rust's 32-bit Arm NEON
//! intrinsics and target features are unstable), so the same binary runs on cores without it,
//! on the scalar kernels. Each must produce exactly the scalar result.

use core::arch::asm;
use std::sync::OnceLock;

use super::SimdBackend;

/// `HWCAP_NEON` on 32-bit Arm Linux.
const HWCAP_NEON: libc::c_ulong = 1 << 12;

/// Whether the CPU has NEON, asked once.
pub(super) fn available() -> bool {
    static HAS_NEON: OnceLock<bool> = OnceLock::new();
    // SAFETY: getauxval only reads the process's auxiliary vector.
    *HAS_NEON.get_or_init(|| unsafe { libc::getauxval(libc::AT_HWCAP) } & HWCAP_NEON != 0)
}

/// 2x2 box average: `dst[i] = (top[2i] + top[2i+1] + bottom[2i] + bottom[2i+1] + 2) >> 2`, 16
/// outputs a step. `None` without NEON or for a row under 16 outputs; the caller finishes the
/// rest.
pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let steps = width / 16;
    if steps == 0 || !available() {
        return None;
    }
    assert!(top.len() >= width * 2 && bottom.len() >= width * 2 && dst.len() >= width);
    // SAFETY: `steps * 16 <= width` outputs: 32 bytes of each source row and 16 of `dst` a
    // step remain (the assert above). `vld2` splits even and odd bytes; the four bytes sum in
    // 16 bits (at most 1020) and `vrshrn #2` adds 2 before the shift, as the scalar kernel
    // rounds.
    unsafe {
        asm!(
            ".fpu neon",
            "2:",
            "vld2.8 {{d0, d1, d2, d3}}, [{t}]!",
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
            "subs {n}, {n}, #1",
            "bne 2b",
            t = inout(reg) top.as_ptr() => _,
            b = inout(reg) bottom.as_ptr() => _,
            d = inout(reg) dst.as_mut_ptr() => _,
            n = inout(reg) steps => _,
            out("d0") _, out("d1") _, out("d2") _, out("d3") _,
            out("d4") _, out("d5") _, out("d6") _, out("d7") _,
            out("d16") _, out("d17") _, out("d18") _, out("d19") _,
            out("d20") _, out("d21") _, out("d22") _, out("d23") _,
            options(nostack),
        );
    }
    Some((SimdBackend::Neon, steps * 16))
}

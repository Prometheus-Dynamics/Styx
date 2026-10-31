//! RISC-V vector (RVV 1.0) kernels, for CPUs whose kernel reports the V extension (the
//! K230D's C908): the vector length is the hardware's, chosen per strip by `vsetvli`, so a
//! kernel covers whole rows. Inline assembly that enables the extension itself (Rust's RVV
//! intrinsics, `target_feature = "v"` and its detection are unstable), so the same binary
//! runs on cores without it, on the scalar kernels. Each must produce exactly the scalar
//! result.

use core::arch::asm;
use std::sync::OnceLock;

use super::SimdBackend;

/// Whether the CPU has the V extension (`AT_HWCAP` bit `'V' - 'A'`), asked once.
pub(super) fn available() -> bool {
    static HAS_V: OnceLock<bool> = OnceLock::new();
    // SAFETY: getauxval only reads the process's auxiliary vector.
    *HAS_V.get_or_init(|| unsafe { libc::getauxval(libc::AT_HWCAP) } & (1 << (b'V' - b'A')) != 0)
}

/// 2x2 box average: `dst[i] = (top[2i] + top[2i+1] + bottom[2i] + bottom[2i+1] + 2) >> 2`.
/// `None` without the V extension.
pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    if !available() {
        return None;
    }
    assert!(top.len() >= width * 2 && bottom.len() >= width * 2 && dst.len() >= width);
    let (mut t, mut b, mut d) = (top.as_ptr(), bottom.as_ptr(), dst.as_mut_ptr());
    let mut left = width;
    while left > 0 {
        let n: usize;
        // SAFETY: `n <= left` pixels: 2n bytes of each source row and n of `dst` remain (the
        // assert above). Segment loads split even and odd bytes (LMUL 2: 32 pixels a strip at
        // VLEN 128); the sums widen to 16 bits
        // (at most 1022 + 2) and narrow back with a shift by 2, as the scalar kernel rounds.
        unsafe {
            asm!(
                ".option push",
                ".option arch, +v",
                "vsetvli {n}, {left}, e8, m2, ta, ma",
                "vlseg2e8.v v0, ({t})",
                "vlseg2e8.v v4, ({b})",
                "vwaddu.vv v8, v0, v2",
                "vwaddu.vv v12, v4, v6",
                "vsetvli zero, {n}, e16, m4, ta, ma",
                "vadd.vv v8, v8, v12",
                "vadd.vi v8, v8, 2",
                "vsetvli zero, {n}, e8, m2, ta, ma",
                "vnsrl.wi v16, v8, 2",
                "vse8.v v16, ({d})",
                ".option pop",
                n = out(reg) n,
                left = in(reg) left,
                t = in(reg) t,
                b = in(reg) b,
                d = in(reg) d,
                out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _,
                out("v5") _, out("v6") _, out("v7") _, out("v8") _, out("v9") _,
                out("v10") _, out("v11") _, out("v12") _, out("v13") _, out("v14") _,
                out("v15") _, out("v16") _, out("v17") _,
                options(nostack),
            );
            t = t.add(2 * n);
            b = b.add(2 * n);
            d = d.add(n);
        }
        left -= n;
    }
    Some((SimdBackend::Rvv, width))
}

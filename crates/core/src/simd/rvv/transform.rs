//! 8x8 tile transposes in RVV 1.0 assembly (see [`crate::simd::TransposeTile`]): output row
//! k is input column k, one strided load (a segment load for 3-byte pixels) at the source
//! stride, gathered backwards when reversed.

use core::arch::asm;

/// # Safety
/// The CPU must have the V extension; 8 rows of 8 `N`-byte pixels readable at `src_stride`
/// and writable at `dst_stride` (see [`crate::simd::scalar::transpose_tile`]).
pub(in crate::simd) unsafe fn transpose_tile<const N: usize>(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    let rev = usize::from(reverse);
    for k in 0..8 {
        // SAFETY: column k of the source tile and row k of the destination, per the caller.
        // `vl` is 8; the gather index `7 - i` is built when `rev` is set.
        unsafe {
            let (s, d) = (src.add(k * N), dst.offset(k as isize * dst_stride));
            macro_rules! row {
                ($($body:literal),* $(,)?) => {
                    asm!(
                        ".option push",
                        ".option arch, +v",
                        $($body,)*
                        ".option pop",
                        s = in(reg) s,
                        ss = in(reg) src_stride,
                        d = in(reg) d,
                        rev = in(reg) rev,
                        out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _,
                        out("v5") _, out("v6") _, out("v7") _, out("v8") _, out("v9") _,
                        options(nostack),
                    )
                };
            }
            match N {
                1 => row!(
                    "vsetivli zero, 8, e8, m1, ta, ma",
                    "vlse8.v v0, ({s}), {ss}",
                    "beqz {rev}, 3f",
                    "vid.v v8",
                    "vrsub.vi v8, v8, 7",
                    "vrgather.vv v4, v0, v8",
                    "vmv.v.v v0, v4",
                    "3:",
                    "vse8.v v0, ({d})",
                ),
                2 => row!(
                    "vsetivli zero, 8, e16, m1, ta, ma",
                    "vlse16.v v0, ({s}), {ss}",
                    "beqz {rev}, 3f",
                    "vid.v v8",
                    "vrsub.vi v8, v8, 7",
                    "vrgather.vv v4, v0, v8",
                    "vmv.v.v v0, v4",
                    "3:",
                    "vse16.v v0, ({d})",
                ),
                3 => row!(
                    "vsetivli zero, 8, e8, m1, ta, ma",
                    "vlsseg3e8.v v0, ({s}), {ss}",
                    "beqz {rev}, 3f",
                    "vid.v v8",
                    "vrsub.vi v8, v8, 7",
                    "vrgather.vv v4, v0, v8",
                    "vrgather.vv v5, v1, v8",
                    "vrgather.vv v6, v2, v8",
                    "vmv.v.v v0, v4",
                    "vmv.v.v v1, v5",
                    "vmv.v.v v2, v6",
                    "3:",
                    "vsseg3e8.v v0, ({d})",
                ),
                _ => row!(
                    "vsetivli zero, 8, e32, m2, ta, ma",
                    "vlse32.v v0, ({s}), {ss}",
                    "beqz {rev}, 3f",
                    "vid.v v8",
                    "vrsub.vi v8, v8, 7",
                    "vrgather.vv v4, v0, v8",
                    "vmv.v.v v0, v4",
                    "3:",
                    "vse32.v v0, ({d})",
                ),
            }
        }
    }
}

//! JPEG leaves in RVV 1.0 assembly (`simd::jpeg`), exact against the scalar functions there.
//!
//! The DCT runs one lane per block: a strip of up to [`CAP`] blocks side by side, each row's
//! samples gathered with strided loads (8 bytes apart), the rows' sums kept in a scratch table
//! between the two passes. The colour leaf takes pixel pairs, with segment loads splitting the
//! six bytes of a pair.

use core::arch::asm;

use super::super::jpeg::FDCT;
use super::super::{ColorLayout, SimdBackend};

/// Most blocks in one strip (the scratch table's lanes).
const CAP: usize = 32;

/// `simd::jpeg::fdct_quant_band` on vectors.
pub(crate) fn fdct_quant(
    band: &[u8],
    stride: usize,
    blocks: usize,
    quant: &[u16; 64],
    out: &mut [i16],
) -> Option<SimdBackend> {
    if blocks == 0 || !super::available() {
        return None;
    }
    let mut scratch = [0i32; 64 * CAP];
    let mut done = 0;
    while done < blocks {
        // SAFETY: the CPU has the V extension; the caller checked that `band` holds 8 rows of
        // `8 * blocks` bytes and `out` 64 coefficients per block.
        done += unsafe {
            fdct_strip(
                band.as_ptr().add(8 * done),
                stride,
                blocks - done,
                quant,
                out.as_mut_ptr().add(64 * done),
                &mut scratch,
            )
        };
    }
    Some(SimdBackend::Rvv)
}

/// One strip: up to `CAP` of the `left` blocks at `src`, coefficients to `out`; returns how
/// many.
///
/// # Safety
/// The V extension; `src` holds 8 rows `stride` apart of `8 * left` bytes, `out` `64 * left`
/// coefficients.
unsafe fn fdct_strip(
    src: *const u8,
    stride: usize,
    left: usize,
    quant: &[u16; 64],
    out: *mut i16,
    scratch: &mut [i32; 64 * CAP],
) -> usize {
    let n: usize;
    // SAFETY: only sets the vector length.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetvli {n}, {left}, e32, m2, ta, ma",
            ".option pop",
            n = out(reg) n,
            left = in(reg) left.min(CAP),
            options(nostack, nomem),
        );
    }
    let coef = FDCT.as_ptr().cast::<i32>();
    let table = scratch.as_mut_ptr();
    // Rows: t[r][u] for the strip's blocks, at scratch[(8r + u)·CAP..].
    for r in 0..8 {
        // SAFETY: loads 8 bytes from each block's row `r`, stores 8 vectors of `n` lanes into
        // the table's row `r`.
        unsafe {
            asm!(
                ".option push",
                ".option arch, +v",
                "vsetvli zero, {n}, e8, mf2, ta, ma",
                "vlse8.v v24, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v25, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v26, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v27, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v28, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v29, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v30, ({p}), {eight}",
                "addi {p}, {p}, 1",
                "vlse8.v v31, ({p}), {eight}",
                "vsetvli zero, {n}, e32, m2, ta, ma",
                "li {k}, 128",
                "vzext.vf4 v8, v24",
                "vsub.vx v8, v8, {k}",
                "vzext.vf4 v10, v25",
                "vsub.vx v10, v10, {k}",
                "vzext.vf4 v12, v26",
                "vsub.vx v12, v12, {k}",
                "vzext.vf4 v14, v27",
                "vsub.vx v14, v14, {k}",
                "vzext.vf4 v16, v28",
                "vsub.vx v16, v16, {k}",
                "vzext.vf4 v18, v29",
                "vsub.vx v18, v18, {k}",
                "vzext.vf4 v20, v30",
                "vsub.vx v20, v20, {k}",
                "vzext.vf4 v22, v31",
                "vsub.vx v22, v22, {k}",
                "li {cnt}, 8",
                "2:",
                "lw {k}, 0({c})",
                "vmul.vx v2, v8, {k}",
                "lw {k}, 4({c})",
                "vmacc.vx v2, {k}, v10",
                "lw {k}, 8({c})",
                "vmacc.vx v2, {k}, v12",
                "lw {k}, 12({c})",
                "vmacc.vx v2, {k}, v14",
                "lw {k}, 16({c})",
                "vmacc.vx v2, {k}, v16",
                "lw {k}, 20({c})",
                "vmacc.vx v2, {k}, v18",
                "lw {k}, 24({c})",
                "vmacc.vx v2, {k}, v20",
                "lw {k}, 28({c})",
                "vmacc.vx v2, {k}, v22",
                "li {k}, 1024",
                "vadd.vx v2, v2, {k}",
                "vsra.vi v2, v2, 11",
                "vse32.v v2, ({t})",
                "addi {c}, {c}, 32",
                "add {t}, {t}, {lane}",
                "addi {cnt}, {cnt}, -1",
                "bnez {cnt}, 2b",
                ".option pop",
                n = in(reg) n,
                p = inout(reg) src.add(r * stride) => _,
                eight = in(reg) 8usize,
                c = inout(reg) coef => _,
                t = inout(reg) table.add(8 * r * CAP) => _,
                lane = in(reg) CAP * 4,
                k = out(reg) _,
                cnt = out(reg) _,
                out("v2") _, out("v3") _, out("v8") _, out("v9") _, out("v10") _,
                out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _,
                out("v21") _, out("v22") _, out("v23") _, out("v24") _, out("v25") _,
                out("v26") _, out("v27") _, out("v28") _, out("v29") _, out("v30") _,
                out("v31") _,
                options(nostack),
            );
        }
    }
    // Columns: F[v][u] quantised, to each block's coefficient `8v + u` (blocks 128 bytes
    // apart).
    for u in 0..8 {
        // SAFETY: reads the table's column `u`, stores one coefficient per block per `v`.
        unsafe {
            asm!(
                ".option push",
                ".option arch, +v",
                "vsetvli zero, {n}, e32, m2, ta, ma",
                "vle32.v v8, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v10, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v12, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v14, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v16, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v18, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v20, ({t})",
                "add {t}, {t}, {row}",
                "vle32.v v22, ({t})",
                "li {cnt}, 8",
                "2:",
                "lw {k}, 0({c})",
                "vmul.vx v2, v8, {k}",
                "lw {k}, 4({c})",
                "vmacc.vx v2, {k}, v10",
                "lw {k}, 8({c})",
                "vmacc.vx v2, {k}, v12",
                "lw {k}, 12({c})",
                "vmacc.vx v2, {k}, v14",
                "lw {k}, 16({c})",
                "vmacc.vx v2, {k}, v16",
                "lw {k}, 20({c})",
                "vmacc.vx v2, {k}, v18",
                "lw {k}, 24({c})",
                "vmacc.vx v2, {k}, v20",
                "lw {k}, 28({c})",
                "vmacc.vx v2, {k}, v22",
                "li {k}, 65536",
                "vadd.vx v2, v2, {k}",
                "vsra.vi v2, v2, 17",
                // sign · ((|F| + Q/2) / Q), with sign the all-ones mask of a negative F.
                "vsra.vi v4, v2, 31",
                "vxor.vv v6, v2, v4",
                "vsub.vv v6, v6, v4",
                "lhu {k}, 0({q})",
                "srli {h}, {k}, 1",
                "vadd.vx v6, v6, {h}",
                "vdivu.vx v6, v6, {k}",
                "vxor.vv v6, v6, v4",
                "vsub.vv v6, v6, v4",
                "vsetvli zero, {n}, e16, m1, ta, ma",
                "vnsra.wi v1, v6, 0",
                "vsse16.v v1, ({o}), {block}",
                "vsetvli zero, {n}, e32, m2, ta, ma",
                "addi {c}, {c}, 32",
                "addi {q}, {q}, 16",
                "addi {o}, {o}, 16",
                "addi {cnt}, {cnt}, -1",
                "bnez {cnt}, 2b",
                ".option pop",
                n = in(reg) n,
                t = inout(reg) table.add(u * CAP) => _,
                row = in(reg) 8 * CAP * 4,
                c = inout(reg) coef => _,
                q = inout(reg) quant.as_ptr().add(u) => _,
                o = inout(reg) out.add(u) => _,
                block = in(reg) 128usize,
                k = out(reg) _,
                h = out(reg) _,
                cnt = out(reg) _,
                out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
                out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _,
                out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _,
                out("v21") _, out("v22") _, out("v23") _,
                options(nostack),
            );
        }
    }
    n
}

/// `simd::jpeg::rgb_ycc420_rows` on vectors, for `pairs` pixel pairs of a 3-byte layout
/// (4-byte layouts: `None`, the scalar function runs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn rgb_ycc420(
    top: &[u8],
    bottom: &[u8],
    y_top: &mut [u8],
    y_bottom: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    pairs: usize,
    layout: ColorLayout,
) -> Option<SimdBackend> {
    if pairs == 0 || layout.bytes_per_pixel() != 3 || !super::available() {
        return None;
    }
    let k = match layout {
        ColorLayout::Rgb24 => &YCC_RGB,
        _ => &YCC_BGR,
    };
    let mut done = 0;
    while done < pairs {
        // SAFETY: the V extension; the dispatcher sized every row for `pairs` pairs.
        done += unsafe {
            ycc_strip(
                top.as_ptr().add(6 * done),
                bottom.as_ptr().add(6 * done),
                y_top.as_mut_ptr().add(2 * done),
                y_bottom.as_mut_ptr().add(2 * done),
                cb.as_mut_ptr().add(done),
                cr.as_mut_ptr().add(done),
                pairs - done,
                k,
            )
        };
    }
    Some(SimdBackend::Rvv)
}

/// Per byte position of a pixel: the luma, Cb and Cr coefficients, then the luma and chroma
/// biases (`simd::jpeg`), for R, G, B in memory.
static YCC_RGB: [i32; 11] = [
    19595,
    38470,
    7471,
    -11059,
    -21709,
    32768,
    32768,
    -27439,
    -5329,
    1 << 15,
    (128 << 18) + (1 << 17) - 1,
];

/// The same for B, G, R in memory.
static YCC_BGR: [i32; 11] = [
    7471,
    38470,
    19595,
    32768,
    -21709,
    -11059,
    -5329,
    -27439,
    32768,
    1 << 15,
    (128 << 18) + (1 << 17) - 1,
];

/// One strip of the `left` pairs; returns how many it did.
///
/// # Safety
/// The V extension; every pointer holds `left` pairs.
#[allow(clippy::too_many_arguments)]
unsafe fn ycc_strip(
    top: *const u8,
    bottom: *const u8,
    y_top: *mut u8,
    y_bottom: *mut u8,
    cb: *mut u8,
    cr: *mut u8,
    left: usize,
    k: &[i32; 11],
) -> usize {
    let n: usize;
    // SAFETY: segment loads of `n` pairs from each row; stores of `2n` luma bytes per row and
    // `n` of each chroma.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetvli {n}, {left}, e8, mf2, ta, ma",
            "vlseg6e8.v v24, ({top})",
            "vlseg6e8.v v16, ({bottom})",
            "vsetvli zero, {n}, e32, m2, ta, ma",
            // Luma of the top row's even pixels (v24..v26) to v2, odd (v27..v29) to v3.
            "lw {a}, 0({k})",
            "lw {b}, 4({k})",
            "lw {c}, 8({k})",
            "lw {h}, 36({k})",
            "vzext.vf4 v8, v24",
            "vmul.vx v12, v8, {a}",
            "vzext.vf4 v8, v25",
            "vmacc.vx v12, {b}, v8",
            "vzext.vf4 v8, v26",
            "vmacc.vx v12, {c}, v8",
            "vadd.vx v12, v12, {h}",
            "vsrl.vi v12, v12, 16",
            "vzext.vf4 v8, v27",
            "vmul.vx v14, v8, {a}",
            "vzext.vf4 v8, v28",
            "vmacc.vx v14, {b}, v8",
            "vzext.vf4 v8, v29",
            "vmacc.vx v14, {c}, v8",
            "vadd.vx v14, v14, {h}",
            "vsrl.vi v14, v14, 16",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vnsrl.wi v10, v12, 0",
            "vnsrl.wi v11, v14, 0",
            "vsetvli zero, {n}, e8, mf2, ta, ma",
            "vnsrl.wi v2, v10, 0",
            "vnsrl.wi v3, v11, 0",
            "vsseg2e8.v v2, ({yt})",
            // The bottom row's (v16..v21) to v4 and v5.
            "vsetvli zero, {n}, e32, m2, ta, ma",
            "vzext.vf4 v8, v16",
            "vmul.vx v12, v8, {a}",
            "vzext.vf4 v8, v17",
            "vmacc.vx v12, {b}, v8",
            "vzext.vf4 v8, v18",
            "vmacc.vx v12, {c}, v8",
            "vadd.vx v12, v12, {h}",
            "vsrl.vi v12, v12, 16",
            "vzext.vf4 v8, v19",
            "vmul.vx v14, v8, {a}",
            "vzext.vf4 v8, v20",
            "vmacc.vx v14, {b}, v8",
            "vzext.vf4 v8, v21",
            "vmacc.vx v14, {c}, v8",
            "vadd.vx v14, v14, {h}",
            "vsrl.vi v14, v14, 16",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vnsrl.wi v10, v12, 0",
            "vnsrl.wi v11, v14, 0",
            "vsetvli zero, {n}, e8, mf2, ta, ma",
            "vnsrl.wi v4, v10, 0",
            "vnsrl.wi v5, v11, 0",
            "vsseg2e8.v v4, ({yb})",
            // The 2x2 sums of each byte position: v8, v12, v14.
            "vsetvli zero, {n}, e32, m2, ta, ma",
            "vzext.vf4 v8, v24",
            "vzext.vf4 v10, v27",
            "vadd.vv v8, v8, v10",
            "vzext.vf4 v10, v16",
            "vadd.vv v8, v8, v10",
            "vzext.vf4 v10, v19",
            "vadd.vv v8, v8, v10",
            "vzext.vf4 v12, v25",
            "vzext.vf4 v10, v28",
            "vadd.vv v12, v12, v10",
            "vzext.vf4 v10, v17",
            "vadd.vv v12, v12, v10",
            "vzext.vf4 v10, v20",
            "vadd.vv v12, v12, v10",
            "vzext.vf4 v14, v26",
            "vzext.vf4 v10, v29",
            "vadd.vv v14, v14, v10",
            "vzext.vf4 v10, v18",
            "vadd.vv v14, v14, v10",
            "vzext.vf4 v10, v21",
            "vadd.vv v14, v14, v10",
            "lw {h}, 40({k})",
            // Cb to v22, Cr to v30.
            "lw {a}, 12({k})",
            "lw {b}, 16({k})",
            "lw {c}, 20({k})",
            "vmul.vx v22, v8, {a}",
            "vmacc.vx v22, {b}, v12",
            "vmacc.vx v22, {c}, v14",
            "vadd.vx v22, v22, {h}",
            "vsra.vi v22, v22, 18",
            "lw {a}, 24({k})",
            "lw {b}, 28({k})",
            "lw {c}, 32({k})",
            "vmul.vx v30, v8, {a}",
            "vmacc.vx v30, {b}, v12",
            "vmacc.vx v30, {c}, v14",
            "vadd.vx v30, v30, {h}",
            "vsra.vi v30, v30, 18",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vnsrl.wi v10, v22, 0",
            "vnsrl.wi v11, v30, 0",
            "vsetvli zero, {n}, e8, mf2, ta, ma",
            "vnsrl.wi v2, v10, 0",
            "vnsrl.wi v3, v11, 0",
            "vse8.v v2, ({cb})",
            "vse8.v v3, ({cr})",
            ".option pop",
            n = out(reg) n,
            left = in(reg) left,
            top = in(reg) top,
            bottom = in(reg) bottom,
            yt = in(reg) y_top,
            yb = in(reg) y_bottom,
            cb = in(reg) cb,
            cr = in(reg) cr,
            k = in(reg) k.as_ptr(),
            a = out(reg) _,
            b = out(reg) _,
            c = out(reg) _,
            h = out(reg) _,
            out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v8") _,
            out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v14") _, out("v15") _, out("v16") _, out("v17") _, out("v18") _,
            out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _,
            out("v29") _, out("v30") _, out("v31") _,
            options(nostack),
        );
    }
    n
}

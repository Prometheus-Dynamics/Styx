//! The half-pel leaves (`simd::h264::hpel`) in RVV 1.0 assembly, exact against the scalar
//! functions there: rows strip-mined at whatever length the hardware gives, the six taps as
//! widening adds and two multiply-accumulates, clipping by a max with zero and saturating
//! narrows.

use core::arch::asm;

/// `simd::h264::hpel_h` on vectors.
pub(in crate::simd) fn hpel_h(src: &[u8], sums: &mut [i16], half: &mut [u8]) -> bool {
    if !super::available() {
        return false;
    }
    if sums.is_empty() {
        return true;
    }
    // SAFETY: the V extension; the dispatcher checked `src` holds `n + 5` samples and `half`
    // `n`, and `sums` is `n` long.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "vsetvli {vl}, {n}, e8, m1, ta, ma",
            "vle8.v v1, ({s})",
            "addi {t}, {s}, 1",
            "vle8.v v2, ({t})",
            "addi {t}, {s}, 2",
            "vle8.v v3, ({t})",
            "addi {t}, {s}, 3",
            "vle8.v v4, ({t})",
            "addi {t}, {s}, 4",
            "vle8.v v5, ({t})",
            "addi {t}, {s}, 5",
            "vle8.v v6, ({t})",
            "vwaddu.vv v8, v1, v6",
            "vwaddu.vv v10, v2, v5",
            "vwaddu.vv v12, v3, v4",
            "vsetvli zero, {vl}, e16, m2, ta, ma",
            "li {t}, 20",
            "vmacc.vx v8, {t}, v12",
            "li {t}, -5",
            "vmacc.vx v8, {t}, v10",
            "vse16.v v8, ({sums})",
            "vadd.vi v14, v8, 15",
            "vadd.vi v14, v14, 1",
            "vsra.vi v14, v14, 5",
            "vmax.vx v14, v14, zero",
            "vsetvli zero, {vl}, e8, m1, ta, ma",
            "vnclipu.wi v16, v14, 0",
            "vse8.v v16, ({half})",
            "add {s}, {s}, {vl}",
            "add {half}, {half}, {vl}",
            "slli {t}, {vl}, 1",
            "add {sums}, {sums}, {t}",
            "sub {n}, {n}, {vl}",
            "bnez {n}, 2b",
            ".option pop",
            n = inout(reg) sums.len() => _,
            s = inout(reg) src.as_ptr() => _,
            sums = inout(reg) sums.as_mut_ptr() => _,
            half = inout(reg) half.as_mut_ptr() => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v14") _, out("v15") _, out("v16") _,
            options(nostack),
        );
    }
    true
}

/// `simd::h264::hpel_v` on vectors.
pub(in crate::simd) fn hpel_v(rows: [&[u8]; 6], out: &mut [u8]) -> bool {
    if !super::available() {
        return false;
    }
    if out.is_empty() {
        return true;
    }
    // SAFETY: the V extension; the dispatcher checked every row holds `out.len()` samples.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "vsetvli {vl}, {n}, e8, m1, ta, ma",
            "vle8.v v1, ({r0})",
            "vle8.v v2, ({r1})",
            "vle8.v v3, ({r2})",
            "vle8.v v4, ({r3})",
            "vle8.v v5, ({r4})",
            "vle8.v v6, ({r5})",
            "vwaddu.vv v8, v1, v6",
            "vwaddu.vv v10, v2, v5",
            "vwaddu.vv v12, v3, v4",
            "vsetvli zero, {vl}, e16, m2, ta, ma",
            "li {t}, 20",
            "vmacc.vx v8, {t}, v12",
            "li {t}, -5",
            "vmacc.vx v8, {t}, v10",
            "vadd.vi v8, v8, 15",
            "vadd.vi v8, v8, 1",
            "vsra.vi v8, v8, 5",
            "vmax.vx v8, v8, zero",
            "vsetvli zero, {vl}, e8, m1, ta, ma",
            "vnclipu.wi v16, v8, 0",
            "vse8.v v16, ({o})",
            "add {r0}, {r0}, {vl}",
            "add {r1}, {r1}, {vl}",
            "add {r2}, {r2}, {vl}",
            "add {r3}, {r3}, {vl}",
            "add {r4}, {r4}, {vl}",
            "add {r5}, {r5}, {vl}",
            "add {o}, {o}, {vl}",
            "sub {n}, {n}, {vl}",
            "bnez {n}, 2b",
            ".option pop",
            n = inout(reg) out.len() => _,
            r0 = inout(reg) rows[0].as_ptr() => _,
            r1 = inout(reg) rows[1].as_ptr() => _,
            r2 = inout(reg) rows[2].as_ptr() => _,
            r3 = inout(reg) rows[3].as_ptr() => _,
            r4 = inout(reg) rows[4].as_ptr() => _,
            r5 = inout(reg) rows[5].as_ptr() => _,
            o = inout(reg) out.as_mut_ptr() => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v16") _,
            options(nostack),
        );
    }
    true
}

/// `simd::h264::hpel_j` on vectors.
pub(in crate::simd) fn hpel_j(rows: [&[i16]; 6], out: &mut [u8]) -> bool {
    if !super::available() {
        return false;
    }
    if out.is_empty() {
        return true;
    }
    // SAFETY: the V extension; the dispatcher checked every row holds `out.len()` sums.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "vsetvli {vl}, {n}, e16, m1, ta, ma",
            "vle16.v v1, ({r0})",
            "vle16.v v2, ({r1})",
            "vle16.v v3, ({r2})",
            "vle16.v v4, ({r3})",
            "vle16.v v5, ({r4})",
            "vle16.v v6, ({r5})",
            "vwadd.vv v8, v1, v6",
            "vwadd.vv v10, v2, v5",
            "vwadd.vv v12, v3, v4",
            "vsetvli zero, {vl}, e32, m2, ta, ma",
            "li {t}, 20",
            "vmacc.vx v8, {t}, v12",
            "li {t}, -5",
            "vmacc.vx v8, {t}, v10",
            "li {t}, 512",
            "vadd.vx v8, v8, {t}",
            "vsra.vi v8, v8, 10",
            "vmax.vx v8, v8, zero",
            "vsetvli zero, {vl}, e16, m1, ta, ma",
            "vnclipu.wi v14, v8, 0",
            "vsetvli zero, {vl}, e8, mf2, ta, ma",
            "vnclipu.wi v15, v14, 0",
            "vse8.v v15, ({o})",
            "slli {t}, {vl}, 1",
            "add {r0}, {r0}, {t}",
            "add {r1}, {r1}, {t}",
            "add {r2}, {r2}, {t}",
            "add {r3}, {r3}, {t}",
            "add {r4}, {r4}, {t}",
            "add {r5}, {r5}, {t}",
            "add {o}, {o}, {vl}",
            "sub {n}, {n}, {vl}",
            "bnez {n}, 2b",
            ".option pop",
            n = inout(reg) out.len() => _,
            r0 = inout(reg) rows[0].as_ptr() => _,
            r1 = inout(reg) rows[1].as_ptr() => _,
            r2 = inout(reg) rows[2].as_ptr() => _,
            r3 = inout(reg) rows[3].as_ptr() => _,
            r4 = inout(reg) rows[4].as_ptr() => _,
            r5 = inout(reg) rows[5].as_ptr() => _,
            o = inout(reg) out.as_mut_ptr() => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _,
            out("v14") _, out("v15") _,
            options(nostack),
        );
    }
    true
}

/// `simd::h264::avg16` on vectors: the averaging add rounding to nearest up (`vxrm` 0).
pub(in crate::simd) fn avg16(
    a: &[u8],
    a_stride: usize,
    b: &[u8],
    b_stride: usize,
    out: &mut [u8; 256],
) -> bool {
    if !super::available() {
        return false;
    }
    // SAFETY: the V extension; the dispatcher checked both blocks are 16 rows of 16 bytes.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "csrwi vxrm, 0",
            "vsetivli zero, 16, e8, m1, ta, ma",
            "li {cnt}, 16",
            "2:",
            "vle8.v v1, ({a})",
            "vle8.v v2, ({b})",
            "vaaddu.vv v3, v1, v2",
            "vse8.v v3, ({o})",
            "add {a}, {a}, {sa}",
            "add {b}, {b}, {sb}",
            "addi {o}, {o}, 16",
            "addi {cnt}, {cnt}, -1",
            "bnez {cnt}, 2b",
            ".option pop",
            a = inout(reg) a.as_ptr() => _,
            b = inout(reg) b.as_ptr() => _,
            o = inout(reg) out.as_mut_ptr() => _,
            sa = in(reg) a_stride,
            sb = in(reg) b_stride,
            cnt = out(reg) _,
            out("v1") _, out("v2") _, out("v3") _,
            options(nostack),
        );
    }
    true
}

//! FP16 leaves of [`crate::simd::half`] in RVV 1.0 assembly with Zvfh (half-precision vector
//! arithmetic; the K230D's C908). Each must match the scalar oracle bit for bit: the same
//! operations in the same order, one rounding each (`vfmacc` is the fused multiply-add of the
//! oracle's `fma`). Lanes are `e16`, strips the hardware's length, and each leaf covers the
//! whole row. Colour sites come from `vid` (the lane index's parity, as strips start on even
//! pixels).
//!
//! # Safety (every function)
//! The CPU must have V and Zvfh ([`available`]); the slices must hold what the scalar kernel
//! reads and writes for `width` pixels.

use std::sync::OnceLock;

use super::{bayer, color, strips};
use crate::format::CfaPattern;
use crate::simd::half::{
    ColourOut, H_16, H_HALF, H_MAX, H_QUARTER, HalfTone, LscRow, quad_positions,
};

/// Whether the fp16 leaves run on this CPU: V (`AT_HWCAP`) and Zvfh.
pub(in crate::simd) fn available() -> bool {
    styx_core::simd::has_rvv() && zvfh()
}

/// Whether the CPU has Zvfh, asked once of Linux's `riscv_hwprobe` (`AT_HWCAP` has single
/// letters only). The kernel reports `RISCV_HWPROBE_EXT_ZVFH` (bit 30 of the IMA extension
/// word, after ZFH and ZFHMIN) in key 4, once it reports the base behaviour IMA in key 3.
/// Before Linux 6.5 the call fails with ENOSYS: the leaves stay off and the oracle runs.
fn zvfh() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    #[repr(C)]
    struct Pair {
        key: i64,
        value: u64,
    }
    const SYS_RISCV_HWPROBE: libc::c_long = 258;
    const KEY_BASE_BEHAVIOR: i64 = 3;
    const KEY_IMA_EXT_0: i64 = 4;
    const BASE_IMA: u64 = 1;
    const EXT_ZVFH: u64 = 1 << 30;
    *HAS.get_or_init(|| {
        let mut pairs = [
            Pair {
                key: KEY_BASE_BEHAVIOR,
                value: 0,
            },
            Pair {
                key: KEY_IMA_EXT_0,
                value: 0,
            },
        ];
        // SAFETY: the kernel writes the two `value`s; no CPU set (count 0, null) is passed.
        let r = unsafe {
            libc::syscall(
                SYS_RISCV_HWPROBE,
                pairs.as_mut_ptr(),
                pairs.len(),
                0usize,
                core::ptr::null_mut::<u64>(),
                0u32,
            )
        };
        r == 0 && pairs[0].value & BASE_IMA != 0 && pairs[1].value & EXT_ZVFH != 0
    })
}

/// Where a colour kernel's strip goes: three planes at the strip, or packed RGB at its first
/// pixel's bytes.
#[derive(Clone, Copy)]
pub(super) enum Dst {
    Planes([*mut u8; 3]),
    Packed(*mut u8),
}

impl Dst {
    /// The assembly's operands: the three plane (or packed) pointers and the packed flag.
    fn ops(self) -> ([*mut u8; 3], usize) {
        match self {
            Dst::Planes(p) => (p, 0),
            Dst::Packed(d) => ([d, d, d], 1),
        }
    }
}

// The tone curve's lookup (see `HalfTone`) on one strip, from the fp16 accumulator at `$acc`
// (at least 16.0) to the bytes at `$out` (e8 mf2, one register). The high byte less 76
// (saturating) indexes both 48-entry tables by a masked indexed load (indices past them read
// nothing and give 0, as `tbl` does); the low byte is the fraction. The output is
// `base + ((slope frac + 128) >> 8)`, exactly `(base 256 + 128 + slope frac) >> 8`.
// Registers v0 (mask), v2..v7. The tables: `{tb}`, `{ts}`; constants `{k76}`, `{k48}`, `{k128}`.
macro_rules! tone {
    ($acc:literal, $out:literal) => {
        concat!(
            "vsetvli zero, {n}, e8, mf2, ta, ma\n",
            "vnsrl.wi v2, ",
            $acc,
            ", 8\n",
            "vssubu.vx v2, v2, {k76}\n",
            "vmsltu.vx v0, v2, {k48}\n",
            "vmv.v.i v4, 0\n",
            "vluxei8.v v4, ({tb}), v2, v0.t\n",
            "vmv.v.i v5, 0\n",
            "vluxei8.v v5, ({ts}), v2, v0.t\n",
            "vnsrl.wi v3, ",
            $acc,
            ", 0\n",
            "vwmulu.vv v6, v5, v3\n",
            "vsetvli zero, {n}, e16, m1, ta, ma\n",
            "vadd.vx v6, v6, {k128}\n",
            "vsrl.vi v6, v6, 8\n",
            "vzext.vf2 v7, v4\n",
            "vadd.vv v6, v6, v7\n",
            "vsetvli zero, {n}, e8, mf2, ta, ma\n",
            "vnsrl.wi ",
            $out,
            ", v6, 0\n",
        )
    };
}

// Stores the three bytes registers v26..v28 (or the one v26) of a strip: planes (`{o0}`..`{o2}`)
// or packed RGB at `{o0}` (`{pk}` nonzero). Labels 8 and 9.
macro_rules! store {
    () => {
        concat!(
            "beqz {pk}, 9f\n",
            "vsseg3e8.v v26, ({o0})\n",
            "j 8f\n",
            "9:\n",
            "vse8.v v26, ({o0})\n",
            "vse8.v v27, ({o1})\n",
            "vse8.v v28, ({o2})\n",
            "8:\n",
        )
    };
}

mod colour;
pub(in crate::simd) use colour::{colour, quad_colour};

/// Runs the colour kernel `strip` (which returns the strip's pixels) over `width` pixels,
/// writing the output's shape: then, for luma and chroma outputs, each strip's luma and
/// chroma from its planes (and the first row's, for chroma), by the integer leaves.
///
/// `strip` writes the pixels it is given (it is the unsafe part: the slices hold `width`).
pub(super) fn emit(out: &mut ColourOut, width: usize, strip: impl Fn(usize, usize, Dst) -> usize) {
    match out {
        ColourOut::Packed(d) => {
            let d = d.as_mut_ptr();
            strips(width, |x, left| {
                strip(x, left, Dst::Packed(d.wrapping_add(3 * x)))
            })
        }
        ColourOut::Planes(p) => {
            let p = p.each_mut().map(|p| p.as_mut_ptr());
            strips(width, |x, left| {
                strip(x, left, Dst::Planes(p.map(|q| q.wrapping_add(x))))
            })
        }
        ColourOut::PlanesLuma(p, y, c) => {
            let p = p.each_mut().map(|p| p.as_mut_ptr());
            let y = y.as_mut_ptr();
            strips(width, |x, left| unsafe {
                let n = strip(x, left, Dst::Planes(p.map(|q| q.wrapping_add(x))));
                let planes = p.map(|q| core::slice::from_raw_parts(q.add(x), n));
                color::rgb_to_y(planes, core::slice::from_raw_parts_mut(y.add(x), n), n, c);
                n
            })
        }
        ColourOut::LumaChroma(p, y, ch, c) => {
            let p = p.each_mut().map(|p| p.as_mut_ptr());
            let y = y.as_mut_ptr();
            let top = ch.top.map(|t| t.as_ptr());
            let u = ch.u.as_mut_ptr();
            let v = ch.v.as_mut().map(|v| v.as_mut_ptr());
            strips(width, |x, left| unsafe {
                let n = strip(x, left, Dst::Planes(p.map(|q| q.wrapping_add(x))));
                let pairs = n / 2;
                let planes = p.map(|q| core::slice::from_raw_parts(q.add(x), n));
                color::rgb_to_y(planes, core::slice::from_raw_parts_mut(y.add(x), n), n, c);
                // The pair's chroma: the first row's planes and this row's, from `x`.
                let tops = top.map(|t| core::slice::from_raw_parts(t.add(x), 2 * pairs));
                let bottoms = planes.map(|q| &q[..2 * pairs]);
                match v {
                    None => color::rgb_to_uv(
                        tops,
                        bottoms,
                        core::slice::from_raw_parts_mut(u.add(x), 2 * pairs),
                        &mut [],
                        pairs,
                        c,
                        true,
                    ),
                    Some(v) => color::rgb_to_uv(
                        tops,
                        bottoms,
                        core::slice::from_raw_parts_mut(u.add(x / 2), pairs),
                        core::slice::from_raw_parts_mut(v.add(x / 2), pairs),
                        pairs,
                        c,
                        false,
                    ),
                }
                n
            })
        }
    }
}

/// Front end of one row of fp16 values, in place (see [`crate::simd::half::front_row`]).
/// `(a + d t)` is the lens shading gain when `lsc` is given.
pub(in crate::simd) unsafe fn front(
    row: &mut [u16],
    black: [u16; 2],
    gain: [u16; 2],
    lsc: Option<LscRow>,
    width: usize,
) -> usize {
    let p = row.as_mut_ptr();
    let (lf, a, d, t) = match lsc {
        Some(l) => (1usize, l.a.as_ptr(), l.d.as_ptr(), usize::from(l.t)),
        None => (0, core::ptr::null(), core::ptr::null(), 0),
    };
    let [b0, b1] = black.map(usize::from);
    let [g0, g1] = gain.map(usize::from);
    strips(width, |i, left| unsafe {
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            "vid.v v4",
            "vand.vi v4, v4, 1",
            "vmsne.vi v0, v4, 0",
            "vmv.v.x v6, {b0}",
            "vmerge.vxm v6, v6, {b1}, v0",
            "vmv.v.x v8, {g0}",
            "vmerge.vxm v8, v8, {g1}, v0",
            "vle16.v v2, ({p})",
            "vor.vx v2, v2, {magic}",
            "vfsub.vv v2, v2, v6",
            "vmv.v.i v10, 0",
            "vfmax.vv v2, v2, v10",
            "vfmul.vv v2, v2, v8",
            "beqz {lf}, 1f",
            "vle16.v v12, ({a})",
            "vle16.v v14, ({d})",
            "vmv.v.x v16, {t}",
            "vfmacc.vv v12, v14, v16",
            "vfmul.vv v2, v2, v12",
            "1:",
            "vmv.v.x v10, {hmax}",
            "vfmin.vv v2, v2, v10",
            "vse16.v v2, ({p})",
        ],
            left = in(reg) left,
            p = in(reg) p.wrapping_add(i),
            a = in(reg) a.wrapping_add(i),
            d = in(reg) d.wrapping_add(i),
            lf = in(reg) lf,
            t = in(reg) t,
            b0 = in(reg) b0,
            b1 = in(reg) b1,
            g0 = in(reg) g0,
            g1 = in(reg) g1,
            magic = in(reg) usize::from(0x6400u16),
            hmax = in(reg) usize::from(H_MAX),
        )
    });
    width
}

/// [`front`] straight from a RAW10 row: the integer unpacking leaf, then the front end on
/// its whole groups of four (the caller finishes the rest).
pub(in crate::simd) unsafe fn front_raw10(
    src: &[u8],
    dst: &mut [u16],
    black: [u16; 2],
    gain: [u16; 2],
    lsc: Option<LscRow>,
    width: usize,
) -> usize {
    let groups = width / 4;
    let done = groups * 4;
    // SAFETY: `src` holds five bytes per group and `dst` four samples per group (the caller).
    unsafe { bayer::unpack_raw10(src, dst, groups) };
    let lsc = lsc.map(|l| LscRow {
        a: &l.a[..done],
        d: &l.d[..done],
        t: l.t,
    });
    // SAFETY: the unpacked samples are `done` long, as the lens shading rows.
    unsafe { front(&mut dst[..done], black, gain, lsc, done) };
    done
}

/// Full-size luma from the mosaic (see [`crate::simd::half::luma_row`]).
pub(in crate::simd) unsafe fn luma(
    rows: [&[u16]; 3],
    dst: &mut [u8],
    width: usize,
    tone: &HalfTone,
) -> usize {
    let [up, cur, dn] = rows.map(|r| r.as_ptr());
    let out = dst.as_mut_ptr();
    let (tb, ts) = (tone.base.as_ptr(), tone.slope.as_ptr());
    strips(width, |x, left| unsafe {
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            "vmv.v.x v21, {quarter}",
            "vmv.v.x v22, {half}",
            "vmv.v.x v23, {h16}",
            // t(i) = (up + dn) / 4 + cur / 2 at columns x, x+1, x+2 in v17, v18, v19.
            "vle16.v v17, ({up})",
            "vle16.v v18, ({dn})",
            "vfadd.vv v17, v17, v18",
            "vfmul.vv v17, v17, v21",
            "vle16.v v18, ({cur})",
            "vfmacc.vv v17, v18, v22",
            "addi {t}, {up}, 2",
            "vle16.v v18, ({t})",
            "addi {t}, {dn}, 2",
            "vle16.v v19, ({t})",
            "vfadd.vv v18, v18, v19",
            "vfmul.vv v18, v18, v21",
            "addi {t}, {cur}, 2",
            "vle16.v v19, ({t})",
            "vfmacc.vv v18, v19, v22",
            "addi {t}, {up}, 4",
            "vle16.v v19, ({t})",
            "addi {t}, {dn}, 4",
            "vle16.v v20, ({t})",
            "vfadd.vv v19, v19, v20",
            "vfmul.vv v19, v19, v21",
            "addi {t}, {cur}, 4",
            "vle16.v v20, ({t})",
            "vfmacc.vv v19, v20, v22",
            // (t(x) + t(x+2)) / 4 + t(x+1) / 2 + 16, through the tone curve.
            "vfadd.vv v20, v17, v19",
            "vfmul.vv v20, v20, v21",
            "vfmacc.vv v20, v18, v22",
            "vfadd.vv v20, v20, v23",
            tone!("v20", "v26"),
            "vse8.v v26, ({o})",
        ],
            left = in(reg) left,
            up = in(reg) up.wrapping_add(x),
            cur = in(reg) cur.wrapping_add(x),
            dn = in(reg) dn.wrapping_add(x),
            o = in(reg) out.wrapping_add(x),
            t = out(reg) _,
            tb = in(reg) tb,
            ts = in(reg) ts,
            k76 = in(reg) 76usize,
            k48 = in(reg) 48usize,
            k128 = in(reg) 128usize,
            quarter = in(reg) usize::from(H_QUARTER),
            half = in(reg) usize::from(H_HALF),
            h16 = in(reg) usize::from(H_16),
        )
    });
    width
}

/// Half-size luma: each quad's mean through the tone curve (see
/// [`crate::simd::half::quad_luma_row`]).
pub(in crate::simd) unsafe fn quad_luma(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u8],
    width: usize,
    tone: &HalfTone,
) -> usize {
    let (top, bottom) = (top.as_ptr(), bottom.as_ptr());
    let out = dst.as_mut_ptr();
    let (tb, ts) = (tone.base.as_ptr(), tone.slope.as_ptr());
    strips(width, |i, left| unsafe {
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            "vlseg2e16.v v17, ({top})",
            "vlseg2e16.v v19, ({bot})",
            "vfadd.vv v17, v17, v18",
            "vfadd.vv v19, v19, v20",
            "vfadd.vv v17, v17, v19",
            "vmv.v.x v21, {quarter}",
            "vmv.v.x v20, {h16}",
            "vfmacc.vv v20, v17, v21",
            tone!("v20", "v26"),
            "vse8.v v26, ({o})",
        ],
            left = in(reg) left,
            top = in(reg) top.wrapping_add(2 * i),
            bot = in(reg) bottom.wrapping_add(2 * i),
            o = in(reg) out.wrapping_add(i),
            tb = in(reg) tb,
            ts = in(reg) ts,
            k76 = in(reg) 76usize,
            k48 = in(reg) 48usize,
            k128 = in(reg) 128usize,
            quarter = in(reg) usize::from(H_QUARTER),
            h16 = in(reg) usize::from(H_16),
        )
    });
    width
}

/// Each quad's R, mean G and B on the 12-bit scale, for the statistics (see
/// [`crate::simd::half::quad_stats_row`]).
pub(in crate::simd) unsafe fn quad_stats(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
    scale: u16,
) -> usize {
    let pos = quad_positions(pattern);
    let (top, bottom) = (top.as_ptr(), bottom.as_ptr());
    let outs = out.map(|o| o.as_mut_ptr());
    strips(width, |i, left| unsafe {
        let [pr, pg1, pg2, pb] = quad_ptrs(top, bottom, i, pos);
        let o = outs.map(|o| o.wrapping_add(i));
        rvvh!([
            "vsetvli {n}, {left}, e16, m1, ta, ma",
            "vlse16.v v17, ({pr}), {s4}",
            "vlse16.v v18, ({pg1}), {s4}",
            "vlse16.v v19, ({pg2}), {s4}",
            "vlse16.v v20, ({pb}), {s4}",
            "vfadd.vv v18, v18, v19",
            "vmv.v.x v21, {half}",
            "vfmul.vv v18, v18, v21",
            "vmv.v.x v22, {scale}",
            "vfmul.vv v17, v17, v22",
            "vfmul.vv v18, v18, v22",
            "vfmul.vv v20, v20, v22",
            "vfcvt.xu.f.v v17, v17",
            "vfcvt.xu.f.v v18, v18",
            "vfcvt.xu.f.v v20, v20",
            "vminu.vx v17, v17, {k4095}",
            "vminu.vx v18, v18, {k4095}",
            "vminu.vx v20, v20, {k4095}",
            "vse16.v v17, ({o0})",
            "vse16.v v18, ({o1})",
            "vse16.v v20, ({o2})",
        ],
            left = in(reg) left,
            pr = in(reg) pr,
            pg1 = in(reg) pg1,
            pg2 = in(reg) pg2,
            pb = in(reg) pb,
            s4 = in(reg) 4usize,
            half = in(reg) usize::from(H_HALF),
            scale = in(reg) usize::from(scale),
            k4095 = in(reg) 4095usize,
            o0 = in(reg) o[0],
            o1 = in(reg) o[1],
            o2 = in(reg) o[2],
        )
    });
    width
}

/// Pointers to the four positions of quad `i` (`[top-left, top-right, bottom-left,
/// bottom-right]`, the order of [`quad_positions`]), each at quad `i`'s sample: a strided
/// load of them reads one quad per element.
pub(super) fn quad_ptrs(
    top: *const u16,
    bottom: *const u16,
    i: usize,
    pos: [usize; 4],
) -> [*const u16; 4] {
    pos.map(|j| {
        let row = if j < 2 { top } else { bottom };
        row.wrapping_add(2 * i + (j & 1))
    })
}

/// `f32` to fp16 bits, rounded to nearest ties to even (as [`crate::simd::f16::from_f32`]),
/// for the finite values and infinities the tables hold.
pub(in crate::simd) unsafe fn from_f32(src: &[f32], dst: &mut [u16]) -> usize {
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    strips(src.len(), |i, left| unsafe {
        rvvh!([
            "vsetvli {n}, {left}, e32, m2, ta, ma",
            "vle32.v v4, ({s})",
            "vsetvli zero, {n}, e16, m1, ta, ma",
            "vfncvt.f.f.w v8, v4",
            "vse16.v v8, ({d})",
        ],
            left = in(reg) left,
            s = in(reg) s.wrapping_add(i),
            d = in(reg) d.wrapping_add(i),
        )
    });
    src.len()
}

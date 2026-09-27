//! Per-frame cost of Styx's SIMD kernels against their scalar references at 1280x720, and the
//! backend each one ran on this machine.

use std::time::Instant;

use styx::core::simd::{self, ColorLayout, Orientation, scalar};

const W: usize = 1280;
const H: usize = 720;

/// Best average of `f` over repeated runs, in milliseconds.
fn time(mut f: impl FnMut()) -> f64 {
    for _ in 0..3 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..10 {
            f();
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e3 / 10.0);
    }
    best
}

fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// Time a row kernel over a whole frame: `(scalar, simd, backend)`.
fn rows(
    src_bpp: usize,
    dst_bpp: usize,
    scalar: impl Fn(&[u8], &mut [u8], usize),
    fast: impl Fn(&[u8], &mut [u8], usize) -> simd::SimdBackend,
) -> (f64, f64, &'static str) {
    let src = bytes(W * H * src_bpp);
    let mut dst = vec![0u8; W * H * dst_bpp];
    let t_scalar = time(|| {
        for y in 0..H {
            scalar(
                &src[y * W * src_bpp..][..W * src_bpp],
                &mut dst[y * W * dst_bpp..][..W * dst_bpp],
                W,
            );
        }
    });
    let mut backend = simd::SimdBackend::Scalar;
    let t_fast = time(|| {
        for y in 0..H {
            backend = fast(
                &src[y * W * src_bpp..][..W * src_bpp],
                &mut dst[y * W * dst_bpp..][..W * dst_bpp],
                W,
            );
        }
    });
    (t_scalar, t_fast, backend.label())
}

fn report(name: &str, (s, f, backend): (f64, f64, &str)) {
    println!(
        "{name:<26} scalar {s:>7.3} ms  simd {f:>7.3} ms  {:>5.1}x  ({backend})",
        s / f
    );
}

fn main() {
    println!("strongest backend: {}", simd::strongest_backend().label());
    report(
        "bgr->rgb",
        rows(3, 3, scalar::swap_rb24_row, simd::swap_rb24_row),
    );
    report(
        "bgra->rgba",
        rows(4, 4, scalar::swap_rb32_row, simd::swap_rb32_row),
    );
    report(
        "bgra->rgb",
        rows(
            4,
            3,
            |s, d, w| scalar::x32_to_rgb24_row(s, d, w, true),
            |s, d, w| simd::x32_to_rgb24_row(s, d, w, true),
        ),
    );
    report(
        "rgb->rgba",
        rows(3, 4, scalar::rgb24_to_rgba_row, simd::rgb24_to_rgba_row),
    );
    report(
        "gray->rgb",
        rows(1, 3, scalar::gray8_to_rgb24_row, simd::gray8_to_rgb24_row),
    );
    report(
        "gray16->rgb",
        rows(
            2,
            3,
            scalar::gray16le_to_rgb24_row,
            simd::gray16le_to_rgb24_row,
        ),
    );
    report(
        "rgb48->rgb",
        rows(
            6,
            3,
            |s, d, w| scalar::rgb48le_to_rgb24_row(s, d, w, false),
            |s, d, w| simd::rgb48le_to_rgb24_row(s, d, w, false),
        ),
    );
    report(
        "yuyv->luma",
        rows(2, 1, scalar::yuyv_luma_row, simd::yuyv_luma_row),
    );
    report(
        "rgb->luma",
        rows(
            3,
            1,
            |s, d, w| scalar::rgb_to_luma_row(s, d, w, ColorLayout::Rgb24),
            |s, d, w| simd::rgb_to_luma_row(s, d, w, ColorLayout::Rgb24),
        ),
    );
    report(
        "bgra->luma",
        rows(
            4,
            1,
            |s, d, w| scalar::rgb_to_luma_row(s, d, w, ColorLayout::Bgra32),
            |s, d, w| simd::rgb_to_luma_row(s, d, w, ColorLayout::Bgra32),
        ),
    );

    // 2x2 box: a 1280x720 grey frame to 640x360.
    let grey = bytes(W * H);
    let mut half = vec![0u8; W / 2 * H / 2];
    let box_scalar = time(|| {
        for y in 0..H / 2 {
            scalar::box2_row(
                &grey[2 * y * W..][..W],
                &grey[(2 * y + 1) * W..][..W],
                &mut half[y * W / 2..][..W / 2],
                W / 2,
            );
        }
    });
    let mut backend = simd::SimdBackend::Scalar;
    let box_fast = time(|| {
        for y in 0..H / 2 {
            backend = simd::box2_row(
                &grey[2 * y * W..][..W],
                &grey[(2 * y + 1) * W..][..W],
                &mut half[y * W / 2..][..W / 2],
                W / 2,
            );
        }
    });
    report("box 2x2 (grey)", (box_scalar, box_fast, backend.label()));

    // Rotations and mirrors against the per-pixel reference.
    for (label, bpp) in [("grey", 1), ("yuyv", 2), ("rgb", 3), ("rgba", 4)] {
        let src = bytes(W * H * bpp);
        for (turns, mirror, what) in [
            (1, false, "rot90"),
            (2, false, "rot180"),
            (0, true, "mirror"),
        ] {
            let o = Orientation::rotation(turns, mirror);
            let (w_out, h_out) = if o.transpose { (H, W) } else { (W, H) };
            let mut dst = vec![0u8; w_out * h_out * bpp];
            let reference = time(|| {
                for y in 0..h_out {
                    for x in 0..w_out {
                        let xm = if mirror { w_out - 1 - x } else { x };
                        let (sx, sy) = match turns {
                            0 => (xm, y),
                            1 => (y, H - 1 - xm),
                            _ => (W - 1 - xm, H - 1 - y),
                        };
                        let s = (sy * W + sx) * bpp;
                        dst[(y * w_out + x) * bpp..][..bpp].copy_from_slice(&src[s..s + bpp]);
                    }
                }
            });
            let mut backend = simd::SimdBackend::Scalar;
            let fast = time(|| {
                backend =
                    simd::transform_packed(&src, W * bpp, &mut dst, w_out * bpp, (W, H), bpp, o);
            });
            report(
                &format!("{what} ({label})"),
                (reference, fast, backend.label()),
            );
        }
    }
}

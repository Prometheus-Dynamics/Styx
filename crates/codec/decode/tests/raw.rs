//! Raw converters against the reference formula (dev's scalar oracle, written out again here),
//! for every colour space, odd and even sizes, padded and tight strides, and one- and two-plane
//! NV12. No dev-recorded pixel vectors exist for these; every expected value is computed here.

mod common;

use common::*;
use styx_core::{CodecError, Converter};
use styx_decode::{RawConverter, Source, Target, yuv::YuvMatrix};
use styx_format::{ColorSpace, FourCc};

const COLORS: [ColorSpace; 5] = [
    ColorSpace::Srgb,
    ColorSpace::Bt709,
    ColorSpace::Bt2020,
    ColorSpace::Bt601,
    ColorSpace::Unknown,
];

/// Packed RGB24 of a `w` x `h` frame from per-pixel Y, U and V samples (chroma already at the
/// pixel's chroma position).
fn expect_rgb(
    w: usize,
    h: usize,
    color: ColorSpace,
    y: impl Fn(usize, usize) -> u8,
    u: impl Fn(usize, usize) -> u8,
    v: impl Fn(usize, usize) -> u8,
) -> Vec<u8> {
    let m = old_for(color);
    let mut out = Vec::with_capacity(w * h * 3);
    for r in 0..h {
        for c in 0..w {
            out.extend_from_slice(&old_rgb(m, y(r, c), u(r, c), v(r, c)));
        }
    }
    out
}

fn convert(
    conv: &mut RawConverter,
    input: &styx_frame::FrameLease<()>,
    w: u32,
    h: u32,
    out_code: FourCc,
) -> Vec<u8> {
    let p = pool(2, w as usize * h as usize * 3);
    let mut out = empty_output(&p, out_code, w, h);
    conv.convert(input, &mut out).expect("converts");
    let bpp = if out_code == FourCc::RG24 { 3 } else { 1 };
    packed_rows(&out, w as usize * bpp, h as usize)
}

#[test]
fn yuv_formula_is_the_old_formula_for_every_value() {
    for (m, old) in [
        (YuvMatrix::BT709, OLD_BT709),
        (YuvMatrix::BT2020, OLD_BT2020),
        (YuvMatrix::BT601_FULL, OLD_BT601_FULL),
        (YuvMatrix::BT601_LIMITED, OLD_BT601_LIMITED),
    ] {
        for y in 0..=255u8 {
            for u in 0..=255u8 {
                for v in 0..=255u8 {
                    assert_eq!(m.rgb(y, u, v), old_rgb(old, y, u, v), "{y} {u} {v}");
                }
            }
        }
    }
}

#[test]
fn nv12_two_planes_match_the_reference_at_odd_and_even_sizes() {
    for color in COLORS {
        for (w, h) in [(5usize, 3usize), (8, 4), (1, 1), (2, 2), (7, 5), (16, 9)] {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (ys, uvs) = (w + 3, cw * 2 + 2); // padded strides
            let y = noise(ys * h, 1 + w as u64);
            let uv = noise(uvs * ch, 9 + h as u64);
            let p = pool(2, ys * h + uvs * ch);
            let input = frame(
                &p,
                FourCc::NV12,
                w as u32,
                h as u32,
                color,
                &[(0, ys, &y), (ys * h, uvs, &uv)],
            );
            let want = expect_rgb(
                w,
                h,
                color,
                |r, c| y[r * ys + c],
                |r, c| uv[r / 2 * uvs + 2 * (c / 2)],
                |r, c| uv[r / 2 * uvs + 2 * (c / 2) + 1],
            );
            let mut conv = RawConverter::new(Source::Nv12, Target::Rgb24).unwrap();
            assert_eq!(
                convert(&mut conv, &input, w as u32, h as u32, FourCc::RG24),
                want,
                "{color:?} {w}x{h}"
            );
        }
    }
}

#[test]
fn nv12_one_plane_matches_the_reference() {
    // Y rows then UV rows in one plane; the chroma row of an odd width holds one byte more.
    for color in COLORS {
        for (w, h) in [(5usize, 3usize), (6, 4)] {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let stride = w.max(cw * 2);
            let total = stride * h + stride * ch;
            let bytes = noise(total, 21 + w as u64);
            let (y, uv) = bytes.split_at(stride * h);
            let p = pool(2, total);
            let input = frame(
                &p,
                FourCc::NV12,
                w as u32,
                h as u32,
                color,
                &[(0, stride, &bytes)],
            );
            let want = expect_rgb(
                w,
                h,
                color,
                |r, c| y[r * stride + c],
                |r, c| uv[r / 2 * stride + 2 * (c / 2)],
                |r, c| uv[r / 2 * stride + 2 * (c / 2) + 1],
            );
            let mut conv = RawConverter::new(Source::Nv12, Target::Rgb24).unwrap();
            assert_eq!(
                convert(&mut conv, &input, w as u32, h as u32, FourCc::RG24),
                want,
                "{color:?} {w}x{h}"
            );
        }
    }
}

#[test]
fn i420_matches_the_reference_at_odd_and_even_sizes() {
    for color in COLORS {
        for (w, h) in [(5usize, 3usize), (8, 4), (3, 3), (16, 10)] {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (ys, cs) = (w + 2, cw + 1);
            let y = noise(ys * h, 31 + w as u64);
            let u = noise(cs * ch, 41 + h as u64);
            let v = noise(cs * ch, 51 + w as u64);
            let p = pool(3, ys * h + 2 * cs * ch);
            let (yo, uo, vo) = (0, ys * h, ys * h + cs * ch);
            let input = frame(
                &p,
                FourCc::I420,
                w as u32,
                h as u32,
                color,
                &[(yo, ys, &y), (uo, cs, &u), (vo, cs, &v)],
            );
            let want = expect_rgb(
                w,
                h,
                color,
                |r, c| y[r * ys + c],
                |r, c| u[r / 2 * cs + c / 2],
                |r, c| v[r / 2 * cs + c / 2],
            );
            let mut conv = RawConverter::new(Source::I420, Target::Rgb24).unwrap();
            assert_eq!(
                convert(&mut conv, &input, w as u32, h as u32, FourCc::RG24),
                want,
                "{color:?} {w}x{h}"
            );
        }
    }
}

#[test]
fn yuyv_matches_the_reference_at_even_sizes_tight_and_padded() {
    for color in COLORS {
        for (w, h, stride) in [(6usize, 3usize, 12usize), (8, 4, 20), (2, 1, 4)] {
            let bytes = noise(stride * h, 61 + w as u64);
            let p = pool(2, bytes.len());
            let input = frame(
                &p,
                FourCc::YUYV,
                w as u32,
                h as u32,
                color,
                &[(0, stride, &bytes)],
            );
            let at = |r: usize, c: usize, k: usize| bytes[r * stride + 4 * (c / 2) + k];
            let want = expect_rgb(
                w,
                h,
                color,
                |r, c| at(r, c, if c % 2 == 0 { 0 } else { 2 }),
                |r, c| at(r, c, 1),
                |r, c| at(r, c, 3),
            );
            let mut conv = RawConverter::new(Source::Yuyv, Target::Rgb24).unwrap();
            assert_eq!(
                convert(&mut conv, &input, w as u32, h as u32, FourCc::RG24),
                want,
                "{color:?} {w}x{h}"
            );
        }
    }
}

#[test]
fn yuyv_odd_width_leaves_the_last_pixel_when_its_group_is_missing() {
    // 3x1 in a tight 6-byte row: the last group needs 4 bytes and the row has only 6 bytes
    // (Y0 U Y1 of 2 pixels), so the third pixel is left as it was (dev's behaviour).
    let bytes = [16u8, 128, 235, 128, 81, 90];
    let p = pool(2, 6);
    let input = frame(&p, FourCc::YUYV, 3, 1, ColorSpace::Bt709, &[(0, 6, &bytes)]);
    let mut conv = RawConverter::new(Source::Yuyv, Target::Rgb24).unwrap();
    let pool_out = self::pool(2, 9);
    let mut out = empty_output(&pool_out, FourCc::RG24, 3, 1);
    {
        let mut w = out.write().unwrap();
        w.bytes().fill(0xEE);
    }
    conv.convert(&input, &mut out).unwrap();
    let got = packed_rows(&out, 9, 1);
    assert_eq!(&got[6..], &[0xEE, 0xEE, 0xEE], "the odd pixel is untouched");
    let m = old_for(ColorSpace::Bt709);
    assert_eq!(&got[..3], &old_rgb(m, 16, 128, 128));
    // Pixel 1 is Y1 of group 0, with group 0's U and V (the 4th byte, 128).
    assert_eq!(&got[3..6], &old_rgb(m, 235, 128, 128));
}

#[test]
fn luma_is_the_y_plane_for_yuyv_and_nv12() {
    let (w, h) = (5usize, 3usize);
    let stride = 12;
    let bytes = noise(stride * h, 71);
    let p = pool(2, bytes.len());
    let input = frame(
        &p,
        FourCc::YUYV,
        w as u32,
        h as u32,
        ColorSpace::Bt709,
        &[(0, stride, &bytes)],
    );
    let mut conv = RawConverter::new(Source::Yuyv, Target::Grey).unwrap();
    let got = convert(&mut conv, &input, w as u32, h as u32, FourCc::GREY);
    let want: Vec<u8> = (0..h)
        .flat_map(|r| (0..w).map(move |c| (r, c)))
        .map(|(r, c)| bytes[r * stride + 2 * c])
        .collect();
    assert_eq!(got, want);

    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let ys = w + 3;
    let y = noise(ys * h, 73);
    let uv = noise(ys * ch, 79);
    let p = pool(2, ys * h + ys * ch);
    let input = frame(
        &p,
        FourCc::NV12,
        w as u32,
        h as u32,
        ColorSpace::Bt709,
        &[(0, ys, &y), (ys * h, ys, &uv)],
    );
    let mut conv = RawConverter::new(Source::Nv12, Target::Grey).unwrap();
    let got = convert(&mut conv, &input, w as u32, h as u32, FourCc::GREY);
    let want: Vec<u8> = (0..h)
        .flat_map(|r| (0..w).map(move |c| (r, c)))
        .map(|(r, c)| y[r * ys + c])
        .collect();
    assert_eq!(got, want);
    let _ = cw;
}

#[test]
fn short_or_wrong_inputs_are_refused_without_panicking() {
    let (w, h) = (5u32, 3u32);
    let mut nv12 = RawConverter::new(Source::Nv12, Target::Rgb24).unwrap();
    let mut yuyv = RawConverter::new(Source::Yuyv, Target::Rgb24).unwrap();
    let mut i420 = RawConverter::new(Source::I420, Target::Rgb24).unwrap();
    let pool_out = pool(2, (w * h * 3) as usize);
    for len in 0..40usize {
        let bytes = noise(len, len as u64 + 1);
        let p = pool(2, len.max(1));
        for conv in [&mut nv12, &mut yuyv, &mut i420] {
            let code = conv.descriptor().input;
            let input = frame(&p, code, w, h, ColorSpace::Bt709, &[(0, 1, &bytes)]);
            let mut out = empty_output(&pool_out, FourCc::RG24, w, h);
            let r = conv.convert(&input, &mut out);
            if let Err(e) = r {
                assert!(
                    matches!(e, CodecError::Rejected(_) | CodecError::Failed(_)),
                    "{e}"
                );
            }
        }
    }
    // A frame of another format is refused as a rejected input.
    let p = pool(2, 64);
    let input = frame(
        &p,
        FourCc::GREY,
        w,
        h,
        ColorSpace::Bt709,
        &[(0, 5, &noise(15, 3))],
    );
    let mut out = empty_output(&pool_out, FourCc::RG24, w, h);
    assert!(matches!(
        nv12.convert(&input, &mut out),
        Err(CodecError::Rejected(_))
    ));
}

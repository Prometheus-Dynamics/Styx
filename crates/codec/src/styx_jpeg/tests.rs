//! The encoder against `jpeg-decoder`: every input kind decodes back to its pixels.

use styx_core::prelude::*;
use styx_core::simd::ColorLayout;
use styx_core::simd::jpeg::rgb_ycc420_rows;

use super::tables::HUFF;
use super::*;
use crate::Codec;

/// A smooth colour image (JPEG's case: little high frequency).
fn rgb(w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&[
                (x * 255 / w.max(64)) as u8,
                (y * 255 / h.max(64)) as u8,
                (((x + y) * 3) % 256) as u8 / 2 + 60,
            ]);
        }
    }
    out
}

fn decode(jpeg: &[u8]) -> (jpeg_decoder::ImageInfo, Vec<u8>) {
    let mut d = jpeg_decoder::Decoder::new(jpeg);
    let pixels = d.decode().expect("decodes");
    (d.info().unwrap(), pixels)
}

fn mean_error(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(&a, &b)| (a as f64 - b as f64).abs())
        .sum::<f64>()
        / a.len() as f64
}

/// I420 planes of `rgb` from the same colour arithmetic as the packed path.
fn i420(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (mut y, mut u, mut v) = (vec![0; w * h], vec![0; cw * ch], vec![0; cw * ch]);
    let mut spare = vec![0; w];
    for p in 0..ch {
        let (r0, r1) = (2 * p, (2 * p + 1).min(h - 1));
        let (top, rest) = y.split_at_mut(r0 * w + w);
        let bottom = if r1 == r0 {
            &mut spare[..]
        } else {
            &mut rest[..w]
        };
        rgb_ycc420_rows(
            &rgb[r0 * w * 3..][..w * 3],
            &rgb[r1 * w * 3..][..w * 3],
            &mut top[r0 * w..],
            bottom,
            &mut u[p * cw..][..cw],
            &mut v[p * cw..][..cw],
            w,
            ColorLayout::Rgb24,
        );
    }
    (y, u, v)
}

#[test]
fn the_annex_k_huffman_tables_are_complete() {
    for spec in &HUFF {
        let codes: usize = spec.bits.iter().map(|&b| b as usize).sum();
        assert_eq!(codes, spec.vals.len());
    }
}

#[test]
fn colour_round_trips_at_every_size() {
    let mut enc = StyxJpeg::new(92);
    let mut jpeg = Vec::new();
    for (w, h) in [
        (1, 1),
        (2, 2),
        (7, 5),
        (16, 16),
        (17, 33),
        (33, 17),
        (64, 48),
        (321, 241),
    ] {
        let pixels = rgb(w, h);
        let input = StyxJpegInput::Packed {
            data: &pixels,
            width: w,
            height: h,
            stride: w * 3,
            layout: ColorLayout::Rgb24,
        };
        enc.encode(&input, &mut jpeg).unwrap();
        let (info, back) = decode(&jpeg);
        assert_eq!((info.width as usize, info.height as usize), (w, h));
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
        let err = mean_error(&pixels, &back);
        assert!(err < 3.0, "{w}x{h}: mean error {err}");
    }
}

#[test]
fn bgr_and_rgba_layouts_match_rgb() {
    let (w, h) = (40, 24);
    let pixels = rgb(w, h);
    let mut enc = StyxJpeg::new(85);
    let mut want = Vec::new();
    let packed = |data, stride, layout| StyxJpegInput::Packed {
        data,
        width: w,
        height: h,
        stride,
        layout,
    };
    enc.encode(&packed(&pixels, w * 3, ColorLayout::Rgb24), &mut want)
        .unwrap();
    let bgr: Vec<u8> = pixels.chunks(3).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let rgba: Vec<u8> = pixels
        .chunks(3)
        .flat_map(|p| [p[0], p[1], p[2], 9])
        .collect();
    let bgra: Vec<u8> = pixels
        .chunks(3)
        .flat_map(|p| [p[2], p[1], p[0], 9])
        .collect();
    let mut got = Vec::new();
    for (data, layout) in [
        (&bgr, ColorLayout::Bgr24),
        (&rgba, ColorLayout::Rgba32),
        (&bgra, ColorLayout::Bgra32),
    ] {
        let stride = w * layout.bytes_per_pixel();
        enc.encode(&packed(data, stride, layout), &mut got).unwrap();
        assert_eq!(got, want, "{layout:?}");
    }
}

#[test]
fn planar_inputs_give_the_packed_bytes() {
    // Whole MCUs, so the padding is the same however the chroma got there.
    let (w, h) = (48, 32);
    let pixels = rgb(w, h);
    let mut enc = StyxJpeg::new(75);
    let (mut want, mut got) = (Vec::new(), Vec::new());
    let input = StyxJpegInput::Packed {
        data: &pixels,
        width: w,
        height: h,
        stride: w * 3,
        layout: ColorLayout::Rgb24,
    };
    enc.encode(&input, &mut want).unwrap();
    let (y, u, v) = i420(&pixels, w, h);
    let i420 = StyxJpegInput::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: w / 2,
    };
    enc.encode(&i420, &mut got).unwrap();
    assert_eq!(got, want, "I420");
    let uv: Vec<u8> = u.iter().zip(&v).flat_map(|(&u, &v)| [u, v]).collect();
    let nv12 = StyxJpegInput::Nv12 {
        y: &y,
        uv: &uv,
        width: w,
        height: h,
        y_stride: w,
        uv_stride: w,
    };
    enc.encode(&nv12, &mut got).unwrap();
    assert_eq!(got, want, "NV12");
}

#[test]
fn odd_planar_sizes_round_trip() {
    let (w, h) = (37, 23);
    let pixels = rgb(w, h);
    let (y, u, v) = i420(&pixels, w, h);
    let mut enc = StyxJpeg::new(95);
    let mut jpeg = Vec::new();
    let input = StyxJpegInput::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: w.div_ceil(2),
    };
    enc.encode(&input, &mut jpeg).unwrap();
    let (_, back) = decode(&jpeg);
    let err = mean_error(&pixels, &back);
    assert!(err < 3.0, "mean error {err}");
}

#[test]
fn grey_round_trips() {
    let (w, h) = (45, 19);
    let grey: Vec<u8> = (0..w * h)
        .map(|i| ((i % w) * 5 + (i / w) * 3) as u8)
        .collect();
    let mut enc = StyxJpeg::new(90);
    let mut jpeg = Vec::new();
    let input = StyxJpegInput::Gray {
        y: &grey,
        width: w,
        height: h,
        stride: w,
    };
    enc.encode(&input, &mut jpeg).unwrap();
    let (info, back) = decode(&jpeg);
    assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::L8);
    let err = mean_error(&grey, &back);
    assert!(err < 2.0, "mean error {err}");
}

#[test]
fn quality_100_is_near_lossless_and_noise_codes() {
    // Noise at quality 100: the largest coefficients and longest runs the coder sees.
    let (w, h) = (32, 32);
    let mut x = 0x2545_F491u32;
    let noise: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let mut enc = StyxJpeg::new(100);
    let mut jpeg = Vec::new();
    let grey = StyxJpegInput::Gray {
        y: &noise[..w * h],
        width: w,
        height: h,
        stride: w,
    };
    enc.encode(&grey, &mut jpeg).unwrap();
    let (_, back) = decode(&jpeg);
    let err = mean_error(&noise[..w * h], &back);
    assert!(err < 1.5, "mean error {err}");
    let colour = StyxJpegInput::Packed {
        data: &noise,
        width: w,
        height: h,
        stride: w * 3,
        layout: ColorLayout::Rgb24,
    };
    enc.encode(&colour, &mut jpeg).unwrap();
    decode(&jpeg);
}

#[test]
fn steady_state_reuses_every_buffer() {
    let (w, h) = (96, 64);
    let pixels = rgb(w, h);
    let mut enc = StyxJpeg::new(80);
    let mut jpeg = Vec::new();
    let input = StyxJpegInput::Packed {
        data: &pixels,
        width: w,
        height: h,
        stride: w * 3,
        layout: ColorLayout::Rgb24,
    };
    enc.encode(&input, &mut jpeg).unwrap();
    let first = jpeg.clone();
    let ptrs = |e: &StyxJpeg, j: &Vec<u8>| {
        (
            e.strips.each_ref().map(|s| s.as_ptr()),
            e.coefs.each_ref().map(|c| c.as_ptr()),
            j.as_ptr(),
        )
    };
    let before = ptrs(&enc, &jpeg);
    enc.encode(&input, &mut jpeg).unwrap();
    assert_eq!(jpeg, first);
    assert_eq!(ptrs(&enc, &jpeg), before, "a buffer was reallocated");
}

#[test]
fn bad_inputs_are_refused() {
    let mut enc = StyxJpeg::new(80);
    let mut jpeg = Vec::new();
    let short = StyxJpegInput::Gray {
        y: &[0; 10],
        width: 4,
        height: 4,
        stride: 4,
    };
    assert!(enc.encode(&short, &mut jpeg).is_err());
    let empty = StyxJpegInput::Gray {
        y: &[],
        width: 0,
        height: 1,
        stride: 0,
    };
    assert!(enc.encode(&empty, &mut jpeg).is_err());
}

#[test]
fn the_codec_encodes_frames() {
    let (w, h) = (20, 14);
    let pixels = rgb(w, h);
    let res = Resolution::new(w as u32, h as u32).unwrap();
    let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
    let mut buf = BufferPool::with_limits(1, pixels.len(), 1).lease();
    buf.resize(pixels.len());
    buf.as_mut_slice().copy_from_slice(&pixels);
    let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), buf, pixels.len(), w * 3);
    let codec = StyxJpegEncoder::new(FourCc::RG24, 90);
    assert_eq!(codec.descriptor().impl_name, "styx-jpeg");
    let out = codec.process_ref(&frame).expect("encodes");
    assert_eq!(out.meta().format.code, FourCc::MJPG);
    let planes = out.planes();
    let (info, back) = decode(planes[0].data());
    assert_eq!((info.width, info.height), (w as u16, h as u16));
    assert!(mean_error(&pixels, &back) < 3.0);
    let wrong = StyxJpegEncoder::new(FourCc::NV12, 90);
    assert!(wrong.process_ref(&frame).is_err());
}

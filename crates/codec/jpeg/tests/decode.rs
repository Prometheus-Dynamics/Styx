//! Standard decoders read every output: `jpeg-decoder` and `zune-jpeg`, both pure Rust, decode
//! each planar input's JPEG to its size with the luma it was given (colour checked through the
//! JFIF conversion, grey directly), at every size with odd edges and at restart-marker settings.

use styx_jpeg::{JpegEncoder, PlanarInput};

const SIZES: [(usize, usize); 8] = [
    (1, 1),
    (2, 2),
    (7, 5),
    (16, 16),
    (17, 33),
    (33, 17),
    (64, 48),
    (321, 241),
];

/// Planes with a smooth gradient and a little texture, and the luma they were made with.
struct Source {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    uv: Vec<u8>,
    w: usize,
    h: usize,
}

fn source(w: usize, h: usize) -> Source {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut seed = 0x2545_f491u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed & 7) as usize
    };
    // Mid-range colour: the decoders' conversion does not clamp, so the luma comes back intact.
    let y: Vec<u8> = (0..w * h)
        .map(|i| (64 + (i % w) * 100 / w.max(1) + (i / w) * 28 / h.max(1) + noise()) as u8)
        .collect();
    let u: Vec<u8> = (0..cw * ch)
        .map(|i| (118 + (i % cw) * 20 / cw.max(1)) as u8)
        .collect();
    let v: Vec<u8> = (0..cw * ch)
        .map(|i| (138 - (i / cw) * 20 / ch.max(1)) as u8)
        .collect();
    let mut uv = vec![0u8; 2 * cw * ch];
    for i in 0..cw * ch {
        uv[2 * i] = u[i];
        uv[2 * i + 1] = v[i];
    }
    Source { y, u, v, uv, w, h }
}

/// Mean absolute error of the decoded luma (BT.601 of the decoded RGB) against the source luma.
fn luma_error(src: &Source, rgb: &[u8]) -> f64 {
    let (w, h) = (src.w, src.h);
    let mut total = 0.0;
    for row in 0..h {
        for col in 0..w {
            let p = &rgb[(row * w + col) * 3..][..3];
            let decoded_y = 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
            total += (decoded_y - src.y[row * w + col] as f64).abs();
        }
    }
    total / (w * h) as f64
}

fn grey_error(src: &Source, luma: &[u8]) -> f64 {
    luma.iter()
        .zip(&src.y)
        .map(|(&a, &b)| (a as f64 - b as f64).abs())
        .sum::<f64>()
        / src.y.len() as f64
}

fn decode_jpeg_decoder(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut d = jpeg_decoder::Decoder::new(jpeg);
    let pixels = d.decode().expect("jpeg-decoder accepts the output");
    let info = d.info().unwrap();
    (info.width, info.height, pixels)
}

fn decode_zune(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut d = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg));
    let pixels = d.decode().expect("zune-jpeg accepts the output");
    let info = d.info().unwrap();
    (info.width, info.height, pixels)
}

/// Decodes `jpeg` with both decoders, checks size and content, and returns the mean error.
fn check(jpeg: &[u8], src: &Source, grey: bool) -> f64 {
    let mut worst = 0.0f64;
    for (label, (w, h, pixels)) in [
        ("jpeg-decoder", decode_jpeg_decoder(jpeg)),
        ("zune-jpeg", decode_zune(jpeg)),
    ] {
        assert_eq!((w as usize, h as usize), (src.w, src.h), "{label} size");
        let err = if grey {
            // A decoder may expand grey to three equal channels: take the first of each pixel.
            let luma: Vec<u8> = match pixels.len() {
                n if n == src.w * src.h => pixels,
                n if n == src.w * src.h * 3 => {
                    pixels.as_chunks::<3>().0.iter().map(|p| p[0]).collect()
                }
                n => panic!("{label} grey output of {n} bytes"),
            };
            grey_error(src, &luma)
        } else {
            assert_eq!(pixels.len(), src.w * src.h * 3, "{label} colour output");
            luma_error(src, &pixels)
        };
        worst = worst.max(err);
    }
    worst
}

fn encode(input: &PlanarInput<'_>, q: u8, restart: bool) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new(q)
        .with_restart_rows(restart)
        .encode(input, &mut out)
        .unwrap();
    out
}

#[test]
fn i420_decodes_at_every_size_and_restart_setting() {
    for (w, h) in SIZES {
        let src = source(w, h);
        let cw = w.div_ceil(2);
        for restart in [false, true] {
            let jpeg = encode(
                &PlanarInput::I420 {
                    y: &src.y,
                    u: &src.u,
                    v: &src.v,
                    width: w,
                    height: h,
                    y_stride: w,
                    c_stride: cw,
                },
                100,
                restart,
            );
            let err = check(&jpeg, &src, false);
            assert!(
                err < 2.0,
                "{w}x{h} restart={restart}: mean luma error {err}"
            );
        }
    }
}

#[test]
fn nv12_decodes_like_i420() {
    for (w, h) in SIZES {
        let src = source(w, h);
        let cw = w.div_ceil(2);
        let jpeg = encode(
            &PlanarInput::Nv12 {
                y: &src.y,
                uv: &src.uv,
                width: w,
                height: h,
                y_stride: w,
                uv_stride: 2 * cw,
            },
            100,
            false,
        );
        let err = check(&jpeg, &src, false);
        assert!(err < 2.0, "{w}x{h}: mean luma error {err}");
    }
}

#[test]
fn grey_decodes_at_every_size() {
    for (w, h) in SIZES {
        let src = source(w, h);
        let jpeg = encode(
            &PlanarInput::Gray {
                y: &src.y,
                width: w,
                height: h,
                stride: w,
            },
            100,
            false,
        );
        let err = check(&jpeg, &src, true);
        assert!(err < 2.0, "{w}x{h}: mean grey error {err}");
    }
}

#[test]
fn quality_trades_error_for_bytes() {
    let (w, h) = (96, 64);
    let src = source(w, h);
    let cw = w / 2;
    let input = PlanarInput::I420 {
        y: &src.y,
        u: &src.u,
        v: &src.v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: cw,
    };
    let (low, high) = (encode(&input, 20, false), encode(&input, 95, false));
    assert!(low.len() < high.len());
    assert!(check(&low, &src, false) > check(&high, &src, false));
}

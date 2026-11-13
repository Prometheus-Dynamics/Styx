//! MJPEG decode: the committed fixture (61x37, 4:2:0, quality 95, a known gradient) against the
//! gradient it was made from, and against `jpeg-decoder`, the reference dev's tests use.
//! Also the registry's choice and the refusals.

mod common;

use common::*;
use styx_core::{CodecError, CodecKind, Decoder, Registry, Request, Verdict};
use styx_decode::MjpegDecoder;
use styx_format::{ColorSpace, FourCc};

const FIXTURE: &[u8] = include_bytes!("data/gradient_61x37_q95_420.jpg");
const W: usize = 61;
const H: usize = 37;

/// The gradient the fixture was encoded from: red along x, green along y, blue 128.
fn source(x: usize, y: usize) -> [u8; 3] {
    [(x * 255 / (W - 1)) as u8, (y * 255 / (H - 1)) as u8, 128]
}

fn jpeg_frame(bytes: &[u8]) -> styx_frame::FrameLease<()> {
    let p = pool(2, bytes.len());
    frame(
        &p,
        FourCc::MJPG,
        W as u32,
        H as u32,
        ColorSpace::Srgb,
        &[(0, 0, bytes)],
    )
}

/// Runs a decoder on the fixture and returns the RGB24 it wrote, packed.
fn decode_with(dec: &mut impl Decoder<()>, bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
    let input = jpeg_frame(bytes);
    let out_pool = pool(2, W * H * 3);
    let mut out = empty_output(&out_pool, FourCc::RG24, W as u32, H as u32);
    dec.decode(&input, &mut out)?;
    assert_eq!(out.format().code, FourCc::RG24);
    assert_eq!(
        (
            out.format().resolution.width.get(),
            out.format().resolution.height.get()
        ),
        (W as u32, H as u32)
    );
    assert_eq!(
        out.id(),
        input.id(),
        "the output takes the input's identity"
    );
    assert_eq!(out.timestamp_ns(), input.timestamp_ns());
    Ok(packed_rows(&out, W * 3, H))
}

fn stats(got: &[u8], want: impl Fn(usize, usize) -> [u8; 3]) -> (f64, u8) {
    let (mut sum, mut max) = (0u64, 0u8);
    for y in 0..H {
        for x in 0..W {
            let w = want(x, y);
            for k in 0..3 {
                let d = got[(y * W + x) * 3 + k].abs_diff(w[k]);
                sum += u64::from(d);
                max = max.max(d);
            }
        }
    }
    (sum as f64 / (W * H * 3) as f64, max)
}

#[test]
fn zune_decodes_the_fixture_close_to_its_source() {
    let mut dec = MjpegDecoder::zune(FourCc::MJPG);
    let got = decode_with(&mut dec, FIXTURE).expect("decodes");
    let (mean, max) = stats(&got, source);
    eprintln!("zune vs source: mean abs error {mean:.3}, max {max}");
    assert!(mean < 2.0 && max <= 12, "mean {mean}, max {max}");
}

#[test]
fn zune_matches_jpeg_decoder_the_reference_dev_uses() {
    let mut reference = jpeg_decoder::Decoder::new(FIXTURE);
    let want = reference.decode().expect("reference decodes");
    let mut dec = MjpegDecoder::zune(FourCc::MJPG);
    let got = decode_with(&mut dec, FIXTURE).expect("decodes");
    let max = got
        .iter()
        .zip(&want)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    let mean = got
        .iter()
        .zip(&want)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / got.len() as f64;
    eprintln!("zune vs jpeg-decoder: mean abs diff {mean:.4}, max {max}");
    // Measured: different IDCT and chroma upsampling in the two libraries, up to 3 levels.
    assert!(
        max <= 4 && mean < 0.5,
        "zune and jpeg-decoder differ by {max} (mean {mean})"
    );
}

#[test]
fn turbojpeg_matches_zune_when_the_library_is_present() {
    let mut turbo = match MjpegDecoder::turbojpeg(FourCc::MJPG) {
        Ok(t) => t,
        Err(CodecError::Unavailable(why)) => {
            eprintln!("SKIPPED turbojpeg comparison: {why}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let mut zune = MjpegDecoder::zune(FourCc::MJPG);
    let a = decode_with(&mut turbo, FIXTURE).expect("turbojpeg decodes");
    let b = decode_with(&mut zune, FIXTURE).expect("zune decodes");
    let max = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
    let (mean_t, max_t) = stats(&a, source);
    eprintln!(
        "turbojpeg vs source: mean {mean_t:.3}, max {max_t}; turbojpeg vs zune max diff {max}"
    );
    assert!(mean_t < 2.0 && max_t <= 12);
    assert!(max <= 3, "turbojpeg and zune differ by {max}");
}

#[test]
fn truncated_and_non_jpeg_input_is_refused() {
    let mut dec = MjpegDecoder::zune(FourCc::MJPG);
    for cut in [0usize, 2, 100, FIXTURE.len() / 2] {
        let r = decode_with(&mut dec, &FIXTURE[..cut]);
        assert!(
            matches!(r, Err(CodecError::Rejected(_)) | Err(CodecError::Failed(_))),
            "cut {cut}: {r:?}"
        );
    }
    let r = decode_with(&mut dec, &noise(500, 5));
    assert!(r.is_err());
}

#[test]
fn registry_tries_turbojpeg_first_and_says_why_it_skips_it() {
    let mut registry: Registry<()> = Registry::new();
    styx_decode::register(&mut registry);
    let req = Request::new(CodecKind::Decoder, FourCc::MJPG, FourCc::RG24);
    let sel = registry.select(req).expect("a decoder for MJPG");
    for d in sel.decisions() {
        eprintln!("{d}");
    }
    let chosen = sel
        .decisions()
        .iter()
        .find(|d| d.verdict == Verdict::Chosen)
        .unwrap();
    let turbo_present = MjpegDecoder::turbojpeg(FourCc::MJPG).is_ok();
    if turbo_present {
        assert_eq!(chosen.descriptor.name, "mjpeg-turbojpeg");
    } else {
        assert_eq!(chosen.descriptor.name, "mjpeg-zune");
        let skipped = sel
            .decisions()
            .iter()
            .find(|d| d.descriptor.name == "mjpeg-turbojpeg")
            .unwrap();
        assert!(
            matches!(skipped.verdict, Verdict::Skipped(_)),
            "{:?}",
            skipped.verdict
        );
    }
    // Selecting any raw converter works from the same registry.
    let raw = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::NV12,
            FourCc::RG24,
        ))
        .expect("an NV12 converter");
    assert_eq!(raw.implementation().descriptor().name, "nv12-to-rgb24");
}

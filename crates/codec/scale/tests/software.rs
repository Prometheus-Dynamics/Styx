//! The software path: the fast kernels equal the scalar reference, hand-checked values, and the
//! registry's choice with software only.

mod support;

use styx_codec_api::{CodecKind, Converter, Registry, Request};
use styx_format::FourCc;
use styx_scale::layout::{FORMATS, convertible};
use styx_scale::{Path, Software, reason, register_software};

/// Converts `input` (a frame of `code_in`) into a new `code_out` frame of `w` x `h`, on `path`.
fn convert(
    path: Path,
    input: &styx_frame::FrameLease<()>,
    code_out: FourCc,
    w: u32,
    h: u32,
) -> Vec<u8> {
    let pool = support::pool(2);
    let mut out = support::blank(&pool, code_out, w, h);
    let mut sw = Software::new(input.format().code, code_out, path);
    sw.convert(input, &mut out).expect("convert");
    support::visible(&out)
}

#[test]
fn fast_path_equals_the_scalar_reference_for_every_pair_and_size() {
    let sizes_in = [(40u32, 24u32), (64, 48), (18, 10), (7, 5)];
    let sizes_out = [(20u32, 12u32), (80, 48), (9, 6), (7, 5), (33, 20), (64, 64)];
    let pool = support::pool(4);
    let mut checked = 0;
    for &code_in in FORMATS.iter() {
        for &code_out in FORMATS.iter() {
            if !convertible(code_in, code_out) {
                continue;
            }
            for &(w, h) in &sizes_in {
                if !styx_scale::layout::fits(code_in, w as usize, h as usize) {
                    continue;
                }
                let input = support::frame(&pool, code_in, w, h, 7 + w + h);
                for &(ow, oh) in &sizes_out {
                    if !styx_scale::layout::fits(code_out, ow as usize, oh as usize) {
                        continue;
                    }
                    let fast = convert(Path::Fast, &input, code_out, ow, oh);
                    let scalar = convert(Path::Scalar, &input, code_out, ow, oh);
                    assert_eq!(
                        fast, scalar,
                        "{code_in} {w}x{h} to {code_out} {ow}x{oh}: fast path differs from the reference"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 300, "only {checked} cases ran");
    println!("fast path equals the scalar reference in {checked} cases");
}

#[test]
fn area_two_to_one_is_the_rounded_block_mean() {
    // 4x4 GREY, rows 0..4: each 2x2 block averaged with rounding half up.
    let pool = support::pool(2);
    let mut input = support::blank(&pool, FourCc::GREY, 4, 4);
    support::set(
        &mut input,
        &[
            0, 255, 10, 20, //
            0, 255, 10, 21, //
            100, 100, 1, 2, //
            101, 100, 3, 4,
        ],
    );
    for path in [Path::Fast, Path::Scalar] {
        let out = convert(path, &input, FourCc::GREY, 2, 2);
        // (0+255+0+255+2)/4 = 128; (10+20+10+21+2)/4 = 15; (100+100+101+100+2)/4 = 100; (1+2+3+4+2)/4 = 3.
        assert_eq!(out, [128, 15, 100, 3], "{path:?}");
    }
}

#[test]
fn bilinear_upscale_matches_the_worked_values() {
    // 2x2 GREY [0 255; 0 255] to 4x2: the first row, worked from the formula in kernel.rs.
    let pool = support::pool(2);
    let mut input = support::blank(&pool, FourCc::GREY, 2, 2);
    support::set(&mut input, &[0, 255, 0, 255]);
    for path in [Path::Fast, Path::Scalar] {
        let out = convert(path, &input, FourCc::GREY, 4, 2);
        assert_eq!(&out[..4], &[0, 64, 191, 255], "{path:?}");
    }
}

#[test]
fn identity_resize_is_a_copy() {
    let pool = support::pool(2);
    let input = support::frame(&pool, FourCc::NV12, 16, 10, 3);
    for path in [Path::Fast, Path::Scalar] {
        assert_eq!(
            convert(path, &input, FourCc::NV12, 16, 10),
            support::visible(&input),
            "{path:?}"
        );
    }
}

#[test]
fn channel_and_chroma_order_conversions_are_exact() {
    let pool = support::pool(8);
    // BGRA pixel (B,G,R,A) = (1,2,3,4) becomes RG24 (R,G,B) = (3,2,1).
    let mut bgra = support::blank(&pool, FourCc::BGRA, 1, 1);
    support::set(&mut bgra, &[1, 2, 3, 4]);
    assert_eq!(convert(Path::Scalar, &bgra, FourCc::RG24, 1, 1), [3, 2, 1]);
    // RG24 (R,G,B) = (9,8,7) becomes BGRA (B,G,R,A) = (7,8,9,255).
    let mut rg = support::blank(&pool, FourCc::RG24, 1, 1);
    support::set(&mut rg, &[9, 8, 7]);
    assert_eq!(convert(Path::Fast, &rg, FourCc::BGRA, 1, 1), [7, 8, 9, 255]);
    // NV12 (Y, U, V) and NV21 (Y, V, U) of one 2x2 frame.
    let mut nv12 = support::blank(&pool, FourCc::NV12, 2, 2);
    support::set(&mut nv12, &[1, 2, 3, 4, 10, 20]);
    assert_eq!(
        convert(Path::Fast, &nv12, FourCc::NV21, 2, 2),
        [1, 2, 3, 4, 20, 10]
    );
    // The same frame as YU12: Y, then all U, then all V.
    assert_eq!(
        convert(Path::Scalar, &nv12, FourCc::YU12, 2, 2),
        [1, 2, 3, 4, 10, 20]
    );
}

#[test]
fn registry_chooses_software_and_says_why() {
    let mut registry: Registry<()> = Registry::new();
    register_software(&mut registry, Path::Fast);
    let selection = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::NV12,
            FourCc::YU12,
        ))
        .expect("selected");
    assert_eq!(
        selection.implementation().descriptor().name,
        "styx-scale-software"
    );
    assert!(reason(&selection).starts_with("styx-scale-software chosen on software"));
    // YUV to RGB needs a colour matrix: nothing is registered for it.
    let err = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::NV12,
            FourCc::RG24,
        ))
        .expect_err("no colour conversion");
    assert!(err.to_string().contains("no converter"), "{err}");
}

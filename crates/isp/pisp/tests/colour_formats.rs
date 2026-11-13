//! Colour and formats: a YUV output's conversion comes from its colour space and range, a YUV
//! colour the library has no conversion for is refused, the settings' outputs must be the
//! builder's, the formats the back end writes are the ones the table names, and the library's
//! sharpening defaults scale by the settings' factors.

use styx_aaa::SharpenParams;
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_pisp::format::{BACK_END_FORMATS, BACK_END_OUTPUTS, pisp_format};
use styx_pisp::pisp::be::defaults::{Encoding, SHARPEN_DEFAULT};
use styx_pisp::pisp::format::{compute_stride_align, formats};
use styx_pisp::pisp::uapi::{BayerOrder, ImageFormatConfig, rgb_enable};
use styx_pisp::{
    BackEndBuilder, Geometry, IspSettings, Output, PispError, back_end::sharpen_scaled,
};

fn input() -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: 1280,
        height: 800,
        format: formats::BAYER16,
        stride: 0,
        stride2: 0,
    };
    compute_stride_align(&mut f, 16);
    f
}

fn output(code: FourCc, w: u32, h: u32, color: ColorSpace, range: ColorRange) -> Output {
    Output {
        media: MediaFormat::new(
            code,
            Resolution::new(w, h).expect("non-empty"),
            color,
            range,
        ),
        stride_align: 64,
    }
}

fn geometry(out0: Output) -> Geometry {
    Geometry {
        input: input(),
        bayer: BayerOrder::Rggb,
        outputs: [
            out0,
            output(FourCc::RGB3, 640, 400, ColorSpace::Srgb, ColorRange::Full),
        ],
        tdn: None,
        slice_at: None,
    }
}

#[test]
fn a_yuv_output_takes_the_conversion_of_its_colour_space_and_range() {
    let cases = [
        (ColorSpace::Bt601, ColorRange::Full, Encoding::Jpeg),
        (ColorSpace::Bt601, ColorRange::Limited, Encoding::Smpte170m),
        (ColorSpace::Bt709, ColorRange::Limited, Encoding::Rec709),
        (ColorSpace::Bt709, ColorRange::Full, Encoding::Rec709Full),
        (ColorSpace::Bt2020, ColorRange::Limited, Encoding::Bt2020),
        (ColorSpace::Bt2020, ColorRange::Full, Encoding::Bt2020Full),
    ];
    for (color, range, encoding) in cases {
        let b = BackEndBuilder::new(&geometry(output(FourCc::NV12, 1280, 800, color, range)))
            .expect("the conversion is one the library has");
        let cfg = &b.config().config;
        assert_eq!(cfg.csc[0], encoding.forward(), "{color:?} {range:?}");
        assert_ne!(cfg.global.rgb_enables & rgb_enable::CSC0, 0, "CSC0 is on");
    }
}

#[test]
fn a_yuv_colour_the_library_cannot_express_is_refused_not_guessed() {
    for (color, range) in [
        (ColorSpace::Srgb, ColorRange::Full),
        (ColorSpace::Unknown, ColorRange::Full),
        (ColorSpace::Bt709, ColorRange::Unknown),
    ] {
        let g = geometry(output(FourCc::NV12, 1280, 800, color, range));
        assert!(
            matches!(BackEndBuilder::new(&g), Err(PispError::Unsupported(_))),
            "{color:?} {range:?} refused"
        );
    }
}

#[test]
fn an_rgb_output_takes_no_conversion_and_a_second_yuv_output_gets_its_own() {
    let b = BackEndBuilder::new(&geometry(output(
        FourCc::NV12,
        1280,
        800,
        ColorSpace::Bt601,
        ColorRange::Full,
    )))
    .unwrap();
    assert_eq!(
        b.config().config.global.rgb_enables & rgb_enable::csc(1),
        0,
        "output 1 is RGB: no conversion"
    );
    let mut g = geometry(output(
        FourCc::NV12,
        1280,
        800,
        ColorSpace::Bt601,
        ColorRange::Full,
    ));
    g.outputs[1] = output(
        FourCc::YUYV,
        640,
        400,
        ColorSpace::Bt709,
        ColorRange::Limited,
    );
    let b = BackEndBuilder::new(&g).expect("two YUV outputs");
    assert_eq!(b.config().config.csc[1], Encoding::Rec709.forward());
    assert_ne!(b.config().config.global.rgb_enables & rgb_enable::csc(1), 0);
}

#[test]
fn settings_outputs_must_be_the_builders_outputs() {
    let g = geometry(output(
        FourCc::NV12,
        1280,
        800,
        ColorSpace::Bt601,
        ColorRange::Full,
    ));
    let mut b = BackEndBuilder::new(&g).unwrap();
    let mut s = IspSettings::neutral(0.0625);
    s.outputs = vec![g.outputs[0].media];
    b.update(&s, 0.01)
        .expect("the builder's own output is accepted");
    s.outputs = vec![
        output(
            FourCc::NV12,
            1280,
            800,
            ColorSpace::Bt709,
            ColorRange::Limited,
        )
        .media,
    ];
    assert!(matches!(b.update(&s, 0.01), Err(PispError::Config(_))));
    s.outputs = vec![g.outputs[0].media, g.outputs[1].media, g.outputs[1].media];
    assert!(matches!(b.update(&s, 0.01), Err(PispError::Config(_))));
}

#[test]
fn the_formats_the_back_end_writes_are_the_table_and_others_are_refused() {
    assert_eq!(pisp_format(FourCc::NV12), Some(formats::NV12));
    assert_eq!(pisp_format(FourCc::RGB3), Some(formats::RGB888));
    assert_eq!(pisp_format(FourCc::YUYV), Some(formats::YUYV));
    assert_eq!(
        pisp_format(FourCc::NV16),
        None,
        "not written by the back end"
    );
    assert!(BACK_END_FORMATS.contains(&FourCc::NV12));
    for cap in BACK_END_OUTPUTS {
        assert_eq!(cap.formats, &BACK_END_FORMATS[..]);
        assert_eq!(cap.max_divisor, 15, "the resampler takes factors under 16x");
    }
    let g = geometry(output(
        FourCc::NV16,
        1280,
        800,
        ColorSpace::Bt601,
        ColorRange::Full,
    ));
    assert!(matches!(
        BackEndBuilder::new(&g),
        Err(PispError::Unsupported(_))
    ));
}

#[test]
fn sharpening_scales_the_library_defaults_and_unit_factors_keep_them() {
    let unit = sharpen_scaled(&SharpenParams {
        threshold: 1.0,
        strength: 1.0,
        limit: 1.0,
    });
    assert_eq!(unit, SHARPEN_DEFAULT);
    let half = sharpen_scaled(&SharpenParams {
        threshold: 0.5,
        strength: 0.5,
        limit: 0.5,
    });
    assert_eq!(
        half.positive_strength,
        SHARPEN_DEFAULT.positive_strength / 2
    );
    assert_eq!(half.positive_limit, SHARPEN_DEFAULT.positive_limit / 2);
    assert_eq!(half.thresholds[0][0], SHARPEN_DEFAULT.thresholds[0][0] / 2);
    assert_eq!(
        half.kernel0, SHARPEN_DEFAULT.kernel0,
        "the kernels are the library's"
    );
}

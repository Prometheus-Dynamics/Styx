use styx_algo::{DenoiseParams, Params, TdnParams};
use styx_pisp::format::{compute_stride_align, formats};
use styx_pisp::uapi::{BayerOrder, ImageFormatConfig};

use super::*;
use crate::isp::{IspSettings, be_template};

fn fmt(w: u16, h: u16, format: u32) -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: w,
        height: h,
        format,
        ..Default::default()
    };
    compute_stride_align(&mut f, 64);
    f
}

fn buffers() -> [Option<ImageFormatConfig>; 2] {
    [
        Some(fmt(1280, 800, formats::NV12)),
        Some(fmt(640, 400, formats::RGB888)),
    ]
}

fn main_builder() -> BeConfigBuilder {
    let t = be_template(
        fmt(1280, 800, formats::BAYER16),
        BayerOrder::Bggr,
        64.0 / 1024.0,
        buffers(),
    )
    .unwrap();
    BeConfigBuilder::new(t).unwrap()
}

fn crop(x: u16, y: u16, w: u16, h: u16) -> BeCropConfig {
    BeCropConfig {
        offset_x: x,
        offset_y: y,
        width: w,
        height: h,
    }
}

fn region(c: BeCropConfig) -> PassSpec {
    PassSpec {
        crop: c,
        output: 0,
        size: None,
        format: None,
    }
}

fn settings() -> IspSettings {
    IspSettings::from_params(&Params::default(), 0, 1.0)
}

/// A region pass is the main config with only its output, cropped at full resolution into
/// the output's buffer stride, over fewer tiles.
#[test]
fn a_region_pass_has_its_own_geometry_and_the_main_blocks() {
    let mut main = main_builder();
    main.update(&settings()).unwrap();
    let mut passes = PassConfigs::new(buffers(), PassTdn::Read);
    passes
        .set(0, Some(region(crop(640, 400, 256, 128))), &main)
        .unwrap();
    let main_tiles = main.config().num_tiles;
    let main_cfg = main.config().config;
    let cfg = passes.config(0, &main).unwrap().unwrap();
    let c = &cfg.config;
    let out = c.output_format[0].image;
    assert_eq!((out.width, out.height, out.stride), (256, 128, 1280));
    assert_eq!(c.global.rgb_enables & rgb_enable::OUTPUT1, 0);
    assert_ne!(c.global.rgb_enables & rgb_enable::OUTPUT0, 0);
    assert!(cfg.num_tiles < main_tiles, "{} tiles", cfg.num_tiles);
    // Every tile reads only around the region.
    for t in &cfg.tiles[..cfg.num_tiles as usize] {
        assert!(t.input_offset_x + t.input_width >= 600 && t.input_offset_x <= 640 + 256);
    }
    assert_eq!(
        (c.blc, c.wbg, c.ccm, c.gamma, c.lsc),
        (
            main_cfg.blc,
            main_cfg.wbg,
            main_cfg.ccm,
            main_cfg.gamma,
            main_cfg.lsc
        )
    );
    // The same spec again prepares nothing.
    let before = passes.prepares();
    passes
        .set(0, Some(region(crop(640, 400, 256, 128))), &main)
        .unwrap();
    assert_eq!(passes.prepares(), before);
}

/// Settings the main config patches reach the passes on the same frame; a main re-prepare
/// (lens shading switched) re-prepares them.
#[test]
fn passes_follow_the_main_config() {
    let mut main = main_builder();
    let mut s = settings();
    main.update(&s).unwrap();
    let mut passes = PassConfigs::new(buffers(), PassTdn::Read);
    passes
        .set(1, Some(region(crop(0, 0, 64, 64))), &main)
        .unwrap();
    assert!(
        passes.config(0, &main).unwrap().is_none(),
        "slot 0 is empty"
    );
    s.wb = [1.9, 1.0, 1.4];
    main.update(&s).unwrap();
    let wbg = main.config().config.wbg;
    assert_eq!(passes.config(1, &main).unwrap().unwrap().config.wbg, wbg);
    let prepared = passes.prepares();
    s.lens_shading = Some(styx_algo::LensShading {
        width: 16,
        height: 12,
        r: vec![1.2; 192],
        g: vec![1.1; 192],
        b: vec![1.3; 192],
    });
    main.update(&s).unwrap();
    let c = passes.config(1, &main).unwrap().unwrap().config;
    assert_eq!(passes.prepares(), prepared + 1);
    assert_ne!(c.global.bayer_enables & bayer_enable::LSC, 0);
    assert_eq!(c.lsc, main.config().config.lsc);
    passes.set(1, None, &main).unwrap();
    assert!(passes.is_empty());
}

/// Passes never write the temporal average: they read the one the main pass wrote, at unit
/// ratio, or go without.
#[test]
fn passes_read_the_temporal_average_without_writing_it() {
    let mut main = main_builder();
    main.enable_tdn(fmt(1280, 800, formats::BAYER16));
    let mut s = settings();
    s.denoise = DenoiseParams {
        tdn: Some(TdnParams {
            noise_constant: 0.0,
            noise_slope: 5.0,
            threshold: 0.08,
        }),
        ..Default::default()
    };
    let mut passes = PassConfigs::new(buffers(), PassTdn::Read);
    passes
        .set(0, Some(region(crop(100, 100, 200, 200))), &main)
        .unwrap();
    for (frame, exposure) in [1.0, 2.0].into_iter().enumerate() {
        main.update_frame(&s, exposure).unwrap();
        let m = main.config().config;
        assert_ne!(m.global.bayer_enables & bayer_enable::TDN_OUTPUT, 0);
        let c = passes.config(0, &main).unwrap().unwrap().config;
        let en = c.global.bayer_enables;
        assert_ne!(en & bayer_enable::TDN, 0, "frame {frame}");
        assert_ne!(en & bayer_enable::TDN_INPUT, 0, "frame {frame}");
        assert_eq!(en & bayer_enable::TDN_OUTPUT, 0, "frame {frame}");
        assert_eq!((c.tdn.reset, c.tdn.ratio), (0, 1 << 14));
        assert_eq!(c.tdn.threshold, m.tdn.threshold);
    }
    passes.set_tdn(PassTdn::Off);
    let c = passes.config(0, &main).unwrap().unwrap().config;
    assert_eq!(
        c.global.bayer_enables & (bayer_enable::TDN | bayer_enable::TDN_INPUT),
        0
    );
    // Without TDN in the main pass, none in the passes either.
    passes.set_tdn(PassTdn::Read);
    s.denoise.tdn = None;
    main.update_frame(&s, 2.0).unwrap();
    let c = passes.config(0, &main).unwrap().unwrap().config;
    assert_eq!(c.global.bayer_enables & bayer_enable::TDN, 0);
}

/// Scaled and reformatted passes, and passes that do not fit the buffers they write into.
#[test]
fn passes_scale_convert_and_must_fit_their_buffers() {
    let mut main = main_builder();
    main.update(&settings()).unwrap();
    let mut passes = PassConfigs::new(buffers(), PassTdn::Off);
    // The whole frame at a quarter on output 1 (its downscaler): a hardware pyramid level.
    let quarter = PassSpec {
        crop: crop(0, 0, 1280, 800),
        output: 1,
        size: Some((320, 200)),
        format: None,
    };
    passes.set(0, Some(quarter), &main).unwrap();
    let c = passes.config(0, &main).unwrap().unwrap().config;
    let out = c.output_format[1].image;
    assert_eq!((out.width, out.height, out.stride), (320, 200, 1920));
    assert_eq!(out.format, formats::RGB888);
    assert_ne!(c.global.rgb_enables & rgb_enable::DOWNSCALE1, 0);
    assert_eq!(c.global.rgb_enables & rgb_enable::OUTPUT0, 0);
    // Half of a region on output 0 (resampler only).
    let half = PassSpec {
        size: Some((128, 64)),
        ..region(crop(64, 64, 256, 128))
    };
    passes.set(1, Some(half), &main).unwrap();
    let c = passes.config(1, &main).unwrap().unwrap().config;
    assert_eq!(c.output_format[0].image.width, 128);
    assert_ne!(c.global.rgb_enables & rgb_enable::RESAMPLE0, 0);
    // RGB into buffers laid out as NV12: the driver places the planes by the buffer.
    let rgb = |w| PassSpec {
        format: Some(formats::RGB888),
        ..region(crop(0, 0, w, 64))
    };
    assert!(
        passes.set(2, Some(rgb(400)), &main).is_err(),
        "NV12 planes, RGB format"
    );
    // Luma alone fits any buffer.
    let luma = PassSpec {
        format: Some(styx_pisp::uapi::image_format::BPS_8),
        ..region(crop(0, 0, 1280, 800))
    };
    passes.set(2, Some(luma), &main).unwrap();
    let c = passes.config(2, &main).unwrap().unwrap().config;
    assert_ne!(c.global.rgb_enables & rgb_enable::CSC0, 0);
    // Larger than output 1's 640x400 buffers, or outside the frame: refused, nothing kept.
    let big = PassSpec {
        output: 1,
        ..region(crop(0, 0, 800, 400))
    };
    assert!(passes.set(3, Some(big), &main).is_err());
    assert!(
        passes
            .set(3, Some(region(crop(1200, 0, 128, 64))), &main)
            .is_err()
    );
    assert!(
        passes
            .set(3, Some(region(crop(1, 0, 64, 64))), &main)
            .is_err()
    );
    assert_eq!(passes.spec(3), None);
}

/// With the main output a crop, the main pass updates the temporal average only around it:
/// passes elsewhere go without; a main crop that jumps elsewhere starts the average over.
#[test]
fn temporal_denoise_only_where_the_main_pass_updated_it() {
    let one = [Some(fmt(1280, 800, formats::NV12)), None];
    let t = be_template(
        fmt(1280, 800, formats::BAYER16),
        BayerOrder::Bggr,
        64.0 / 1024.0,
        one,
    )
    .unwrap();
    let mut main = BeConfigBuilder::new(t).unwrap();
    main.enable_tdn(fmt(1280, 800, formats::BAYER16));
    let mut s = settings();
    s.denoise.tdn = Some(TdnParams {
        noise_constant: 0.0,
        noise_slope: 5.0,
        threshold: 0.08,
    });
    main.set_output_crop(0, Some(crop(0, 0, 512, 400))).unwrap();
    main.update_frame(&s, 1.0).unwrap();
    main.update_frame(&s, 1.0).unwrap();
    let reads = |c: &BeConfig| c.global.bayer_enables & bayer_enable::TDN_INPUT != 0;
    assert!(reads(&main.config().config));
    let mut passes = PassConfigs::new(one, PassTdn::Read);
    passes
        .set(0, Some(region(crop(128, 128, 128, 128))), &main)
        .unwrap();
    passes
        .set(1, Some(region(crop(900, 500, 128, 128))), &main)
        .unwrap();
    assert!(reads(&passes.config(0, &main).unwrap().unwrap().config));
    assert!(!reads(&passes.config(1, &main).unwrap().unwrap().config));
    // A small move keeps the average; a jump starts it over.
    main.set_output_crop(0, Some(crop(16, 0, 512, 400)))
        .unwrap();
    main.update_frame(&s, 1.0).unwrap();
    assert!(reads(&main.config().config));
    main.set_output_crop(0, Some(crop(700, 380, 512, 400)))
        .unwrap();
    main.update_frame(&s, 1.0).unwrap();
    let c = main.config().config;
    assert!(!reads(&c));
    assert_eq!(c.tdn.reset, 1);
}

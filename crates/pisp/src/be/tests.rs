use super::*;
use crate::format::formats;

fn bayer16(w: u16, h: u16) -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: w,
        height: h,
        format: formats::BAYER16,
        ..Default::default()
    };
    crate::format::compute_stride_align(&mut f, 64);
    f
}

fn simple(format: u32) -> BackEnd {
    BackEnd::simple_bayer(
        bayer16(1280, 800),
        BayerOrder::Bggr,
        4096,
        (1.5, 1.0, 1.8),
        None,
        format,
    )
}

#[test]
fn nv12_full_size() {
    let mut be = simple(formats::NV12);
    let t = be.prepare().unwrap();
    let c = &t.config;
    assert_eq!(t.num_tiles, 3);
    let o = c.output_format[0];
    assert_eq!((o.image.width, o.image.height), (1280, 800));
    assert_eq!((o.image.stride, o.image.stride2), (1280, 1280));
    assert_eq!((o.hi, o.hi2), (65535, 65535));
    assert_ne!(c.global.rgb_enables & rgb_enable::CSC0, 0);
    assert_eq!(c.global.rgb_enables & rgb_enable::RESAMPLE0, 0);
    assert_eq!(c.wbg.gain_r, 1536);
    assert_eq!(c.blc.black_level_gr, 4096);
    // Tiles cover the output exactly once, left to right.
    let mut x = 0;
    for tile in &t.tiles[..3] {
        assert_eq!(tile.output_offset_x[0], x);
        assert_eq!(
            tile.input_width - tile.crop_x_start[0] - tile.crop_x_end[0],
            tile.output_width[0]
        );
        assert_eq!(tile.output_addr_offset[0], u32::from(x));
        assert_eq!(tile.input_addr_offset, u32::from(tile.input_offset_x) * 2);
        assert!(tile.input_width <= 640);
        x += tile.output_width[0];
    }
    assert_eq!(x, 1280);
    assert_eq!(
        t.tiles[0].edge,
        TILE_LEFT_EDGE | TILE_TOP_EDGE | TILE_BOTTOM_EDGE
    );
    assert_eq!(
        t.tiles[2].edge,
        TILE_RIGHT_EDGE | TILE_TOP_EDGE | TILE_BOTTOM_EDGE
    );
    assert_eq!(t.as_bytes().len(), 16720);
}

#[test]
fn rgb_output_has_no_csc() {
    let mut be = simple(formats::RGB888);
    let t = be.prepare().unwrap();
    assert_eq!(t.config.global.rgb_enables & rgb_enable::CSC0, 0);
    assert_eq!(t.config.output_format[0].image.stride, 3840);
}

#[test]
fn smart_resize_uses_downscaler_on_output1() {
    let mut be = simple(formats::NV12);
    let g = be.config().global;
    be.set_global(
        g.bayer_enables,
        g.rgb_enables | rgb_enable::OUTPUT1,
        BayerOrder::Bggr,
    );
    be.set_output_format(
        1,
        BeOutputFormatConfig {
            image: ImageFormatConfig {
                format: formats::YUV420P,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    be.set_smart_resize(1, 320, 200);
    be.set_smart_resize(0, 960, 600);
    let t = be.prepare().unwrap();
    let c = &t.config;
    assert_ne!(c.global.rgb_enables & rgb_enable::DOWNSCALE1, 0);
    assert_ne!(c.global.rgb_enables & rgb_enable::RESAMPLE1, 0);
    assert_ne!(c.global.rgb_enables & rgb_enable::RESAMPLE0, 0);
    assert_eq!(c.global.rgb_enables & rgb_enable::DOWNSCALE0, 0);
    assert_eq!(c.downscale[1].scale_factor_h, 2 << 12);
    let o1 = c.output_format[1].image;
    assert_eq!((o1.width, o1.height), (320, 200));
    let (mut w0, mut w1) = (0, 0);
    for tile in &t.tiles[..t.num_tiles as usize] {
        if tile.output_offset_y[0] == 0 {
            w0 += tile.output_width[0];
        }
        if tile.output_offset_y[1] == 0 {
            w1 += tile.output_width[1];
        }
    }
    assert_eq!((w0, w1), (960, 320));
}

#[test]
fn bad_configs_are_rejected() {
    let mut be = simple(formats::NV12);
    be.set_global(0, rgb_enable::OUTPUT0, BayerOrder::Bggr);
    assert!(be.prepare().is_err());
    let mut be = simple(formats::NV12);
    be.set_input_format(bayer16(1279, 800));
    assert!(be.prepare().is_err());
    let mut be = simple(formats::NV12);
    let g = be.config().global;
    be.set_global(
        g.bayer_enables,
        g.rgb_enables & !rgb_enable::OUTPUT0,
        BayerOrder::Bggr,
    );
    assert!(be.prepare().is_err());
}

#[test]
fn narrow_tiles_when_limited() {
    let mut be = simple(formats::NV12);
    be.set_max_tile_width(256);
    let t = be.prepare().unwrap();
    assert!(t.num_tiles >= 6);
    assert!(
        t.tiles[..t.num_tiles as usize]
            .iter()
            .all(|t| t.input_width <= 256)
    );
}

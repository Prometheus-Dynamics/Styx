//! Field-level expectations, written as numbers, not read back from the library.

use styx_format::FourCc;
use styx_isp600::gaps::{self, GAPS, MESSAGE};
use styx_isp600::job::{self, JOB_BYTES};
use styx_isp600::layout;
use styx_isp600::library::layout::{LOAD_BYTES, STATISTICS_BYTES, check_load, module, reg};
use styx_isp600::library::tuning::{self, ChannelExpand, expand};
use styx_isp600::load::{self, EncodeError, LoadImage, Options};
use styx_isp600::stats::StatisticsView;
use styx_settings::IspSettings;

fn sharpen() -> styx_aaa::SharpenParams {
    styx_aaa::SharpenParams {
        threshold: 0.5,
        strength: 1.0,
        limit: 2.0,
    }
}

/// The loaded register word at `register` (from the register image's loaded registers).
fn word(image: &LoadImage, register: usize) -> u32 {
    // LOAD_REGISTERS_OFFSET (0x100) + the register's offset.
    let at = 0x100 + register;
    u32::from_le_bytes(image.bytes()[at..at + 4].try_into().unwrap())
}

#[test]
fn the_load_image_is_the_five_regions_the_kernel_copies_whole() {
    assert_eq!(LOAD_BYTES, 0x7700);
    let expect = [
        (0x0, 0x1000),
        (0x1000, 0x1800),
        (0x2800, 0x2600),
        (0x4e00, 0x1200),
        (0x6000, 0x1700),
    ];
    for (n, (offset, bytes)) in expect.into_iter().enumerate() {
        let r = layout::load_region(n as u8).expect("region");
        assert_eq!((r.offset, r.bytes), (offset, bytes), "load region {n}");
    }
    assert!(layout::load_region(5).is_none());
}

#[test]
fn the_statistics_buffer_is_the_saved_part_then_the_save_load_part() {
    assert_eq!(STATISTICS_BYTES, 0x9d00);
    let histogram0 = layout::statistics_region(10).expect("histogram 0");
    assert_eq!((histogram0.offset, histogram0.bytes), (0x5a80, 0x400));
    // Saved registers and statistics end at SAVE_BYTES (0x6a00), where the save-load part starts.
    let tone = layout::statistics_region(12).expect("tone luminance");
    assert_eq!(tone.offset + tone.bytes, 0x6a00);
    let pkx = layout::statistics_region(13).expect("tone pkx");
    assert_eq!((pkx.offset, pkx.bytes), (0x6a00, 0x3000));
    let k = layout::statistics_region(14).expect("denoise k");
    assert_eq!((k.offset, k.bytes), (0x9a00, 0x300));
    assert!(layout::statistics_region(15).is_none());
}

#[test]
fn info_states_the_counts_and_sizes_the_library_fixes() {
    let info = layout::info(2);
    assert_eq!(info.layout_version, 1);
    assert_eq!(info.load_bytes, 0x7700);
    assert_eq!(info.statistics_bytes, 0x9d00);
    assert_eq!(info.load_regions, 5);
    assert_eq!(info.statistics_regions, 15);
    assert_eq!(info.settle_frames, 2);
}

#[test]
fn the_module_enable_word_sits_at_the_loaded_register_offset() {
    // LOAD_REGISTERS_OFFSET (0x100) + MODULE_BYPASS1 (0x34).
    assert_eq!(0x100 + reg::MODULE_BYPASS1, 0x134);
    assert_eq!(module::AE, 1 << 24);
    assert_eq!(module::HIST1, 1 << 29);
}

#[test]
fn a_neutral_image_enables_nothing_but_the_bit_depths() {
    let image = load::encode(
        &IspSettings::neutral(0.0),
        FourCc::RGGB10P,
        Options::default(),
    )
    .expect("neutral settings encode");
    // No black level: the enable is clear, the input and output bit depths (10) are set.
    assert_eq!(word(&image, reg::CH0_EXPAND_CFG0), (10 << 4) | (10 << 16));
    assert_eq!(word(&image, reg::MODULE_BYPASS1), 0);
    assert_eq!(word(&image, reg::D3D_CFG0), 0);
    assert!(check_load(&image.bytes()[..]).is_ok());
}

#[test]
fn a_black_level_encodes_the_library_words_with_the_input_bit_depth() {
    let image = load::encode(
        &IspSettings::neutral(0.05),
        FourCc::RGGB10P,
        Options::default(),
    )
    .expect("black level encodes");
    // Enable (bit 0) and inverse (bit 1, off), input bits 10 at bit 4, output bits 10 at bit 16.
    let channel = 1 | (10 << 4) | (10 << 16);
    assert_eq!(channel, 0x000a_00a1);
    for register in [
        reg::CH0_EXPAND_CFG0,
        reg::CH1_EXPAND_CFG0,
        reg::CH2_EXPAND_CFG0,
    ] {
        assert_eq!(word(&image, register), channel, "register {register:#x}");
    }
    assert_eq!(word(&image, reg::MODULE_BYPASS1), module::BLC);
    assert_eq!(word(&image, reg::D3D_CFG0), 0);
}

#[test]
fn the_image_is_the_library_encoding_word_for_word() {
    let image = load::encode(
        &IspSettings::neutral(0.05),
        FourCc::RGGB10P,
        Options {
            temporal_record: true,
        },
    )
    .expect("encodes");
    // The same writes through the library directly, from zero.
    let mut expected = Box::new([0u8; LOAD_BYTES]);
    {
        let mut library = tuning::Image::new(&mut expected);
        for channel in 0..expand::CHANNELS {
            library
                .set_channel_expand(
                    channel,
                    ChannelExpand {
                        black_level: true,
                        inverse_black_level: false,
                        input_bits: 10,
                        output_bits: 10,
                    },
                )
                .unwrap();
        }
        library.set_modules(module::BLC).unwrap();
        library.set_temporal_record(true);
    }
    assert_eq!(image.bytes()[..], expected[..]);
    assert_eq!(word(&image, reg::D3D_CFG0), 1);
    assert!(check_load(&image.bytes()[..]).is_ok());
}

#[test]
fn a_black_level_on_one_cfa_channel_enables_all_three_channels() {
    let mut settings = IspSettings::neutral(0.0);
    settings.black_channels = Some([0.0, 0.0, 0.0, 0.02]);
    let image = load::encode(&settings, FourCc::RGGB10P, Options::default()).expect("encodes");
    for register in [
        reg::CH0_EXPAND_CFG0,
        reg::CH1_EXPAND_CFG0,
        reg::CH2_EXPAND_CFG0,
    ] {
        assert_eq!(word(&image, register) & expand::BLC, expand::BLC);
    }
    assert_eq!(word(&image, reg::MODULE_BYPASS1), module::BLC);
}

#[test]
fn the_bit_depth_comes_from_the_input_format() {
    let eight = load::encode(&IspSettings::neutral(0.1), FourCc::GRBG, Options::default())
        .expect("8-bit encodes");
    assert_eq!(word(&eight, reg::CH0_EXPAND_CFG0), 1 | (8 << 4) | (8 << 16));
    let not_raw = load::encode(&IspSettings::neutral(0.1), FourCc::NV12, Options::default());
    assert!(matches!(not_raw, Err(EncodeError::Input(code)) if code == FourCc::NV12));
}

#[test]
fn every_unavailable_setting_is_refused_by_name_and_writes_nothing() {
    type Set = fn(&mut IspSettings);
    let cases: [(&str, Set); 10] = [
        ("wb", |s| s.wb = [1.5, 1.0, 1.2]),
        ("digital_gain", |s| s.digital_gain = 1.25),
        ("flicker", |s| s.flicker = 1.1),
        ("flicker_bands", |s| s.flicker_bands = Some(vec![1.0, 1.1])),
        ("ccm", |s| {
            s.ccm = [1.1, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
        }),
        ("gamma", |s| s.gamma = Some(Default::default())),
        ("lens_shading", |s| {
            s.lens_shading = Some(Default::default());
        }),
        ("denoise", |s| s.denoise.noise_slope = 0.5),
        ("sharpen", |s| s.sharpen = Some(sharpen())),
        ("histogram_weights", |s| {
            s.histogram_weights = Some(Default::default());
        }),
    ];
    for (name, set) in cases {
        let mut settings = IspSettings::neutral(0.05);
        set(&mut settings);
        let error =
            load::encode(&settings, FourCc::RGGB10P, Options::default()).expect_err("refused");
        match &error {
            EncodeError::Unavailable(refused) => assert_eq!(refused.setting, name),
            other => panic!("{name}: expected Unavailable, got {other:?}"),
        }
        let text = error.to_string();
        assert!(text.starts_with(MESSAGE), "{name}: {text}");
        assert!(text.contains(name), "{name}: {text}");
    }
}

#[test]
fn the_message_is_the_stated_one() {
    assert_eq!(
        MESSAGE,
        "hardware ISP tuning unavailable: no open source for the encodings"
    );
    let mut settings = IspSettings::neutral(0.0);
    settings.wb = [2.0, 1.0, 1.0];
    let error = gaps::check(&settings).expect_err("wb");
    assert_eq!(
        error.to_string(),
        "hardware ISP tuning unavailable: no open source for the encodings (wb)"
    );
}

#[test]
fn a_setting_refused_first_is_the_first_in_block_order() {
    // Two unavailable settings: the first in the order of the load image (white balance) names it.
    let mut settings = IspSettings::neutral(0.0);
    settings.sharpen = Some(sharpen());
    settings.wb = [2.0, 1.0, 1.0];
    assert_eq!(gaps::check(&settings).unwrap_err().setting, "wb");
    assert_eq!(gaps::unavailable(&settings).count(), 2);
}

#[test]
fn a_black_level_alone_is_supported_and_neutral_settings_need_no_encoding() {
    assert_eq!(gaps::unavailable(&IspSettings::neutral(0.0)).count(), 0);
    assert!(gaps::check(&IspSettings::neutral(0.3)).is_ok());
    assert!(GAPS.iter().any(|g| g.block.starts_with("WB")));
    assert_eq!(GAPS.len(), 8);
}

#[test]
fn statistics_are_read_by_region_and_refused_when_short() {
    let bytes = vec![0x5au8; STATISTICS_BYTES];
    let view = StatisticsView::new(&bytes).expect("full buffer");
    assert_eq!(view.region(10).unwrap().len(), 0x400);
    assert_eq!(view.region(14).unwrap().len(), 0x300);
    assert!(view.region(15).is_none());
    assert!(StatisticsView::new(&bytes[..STATISTICS_BYTES - 1]).is_none());
}

#[test]
fn a_job_names_the_load_layout_and_its_buffers() {
    let j = job::job(7, 1, Some(2));
    assert_eq!(j.layout, 1);
    assert_eq!(j.identity, 7);
    assert_eq!(j.config_buffer, 1);
    assert_eq!(j.input_buffer, u32::MAX);
    assert_eq!(j.outputs, [2, u32::MAX, u32::MAX, u32::MAX]);
    let skipped = job::job(8, 0, None);
    assert_eq!(skipped.outputs[0], u32::MAX);
    let (_, used) = job::encode(&j).unwrap();
    assert!(used <= JOB_BYTES);
}

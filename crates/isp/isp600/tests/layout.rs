//! Field-level expectations, written as numbers, not read back from the library.

use styx_isp600::gaps::{self, GAPS};
use styx_isp600::job::{self, JOB_BYTES};
use styx_isp600::layout;
use styx_isp600::library::layout::{LOAD_BYTES, STATISTICS_BYTES, check_load, module, reg};
use styx_isp600::load::{LoadImage, WriteError};
use styx_isp600::stats::StatisticsView;
use styx_settings::IspSettings;

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
fn a_fresh_image_passes_the_check_and_an_address_word_refuses_it() {
    let mut image = LoadImage::new();
    assert!(image.checked().is_ok());
    // Control block 0's load address: AHB_OFFSET (0x20) + LOAD_ADDR (0x4).
    image.set_word(0x24, 1).unwrap();
    assert_eq!(
        image.checked().err(),
        Some(styx_isp600::library::layout::Refusal::Address(0x24))
    );
    // Temporal denoise's bayer address: LOAD_REGISTERS_OFFSET (0x100) + D3D_BAYER_ADDR (0x40).
    let mut other = LoadImage::new();
    other.set_word(0x140, 1).unwrap();
    assert!(other.checked().is_err());
    // The library's own check gives the same answer on the same bytes.
    assert!(check_load(&other.bytes()[..]).is_err());
}

#[test]
fn writes_land_at_their_offsets_and_module_bits_keep_the_rest() {
    let mut image = LoadImage::new();
    image.set_modules(module::BLC | module::WB, true).unwrap();
    image.set_modules(module::WB, false).unwrap();
    let word = u32::from_le_bytes(image.bytes()[0x134..0x138].try_into().unwrap());
    assert_eq!(word, module::BLC);
    assert_eq!(image.set_word(0x1000, 0), Err(WriteError::Register(0x1000)));
    assert_eq!(image.set_word(0x2, 0), Err(WriteError::Register(0x2)));
}

#[test]
fn a_table_must_be_its_region_size_and_the_register_image_is_not_a_table() {
    let mut image = LoadImage::new();
    let fe = vec![0xabu8; 0x1800];
    image.set_table(1, &fe).unwrap();
    assert_eq!(image.bytes()[0x1000], 0xab);
    assert_eq!(image.bytes()[0x27ff], 0xab);
    assert_eq!(image.bytes()[0x2800], 0);
    assert_eq!(
        image.set_table(1, &fe[..10]),
        Err(WriteError::TableSize {
            region: 1,
            expected: 0x1800,
            given: 10
        })
    );
    assert_eq!(image.set_table(0, &fe), Err(WriteError::Region(0)));
    assert_eq!(image.set_table(5, &fe), Err(WriteError::Region(5)));
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

#[test]
fn neutral_settings_need_no_encoding_and_a_gain_names_its_block() {
    let neutral = IspSettings::neutral(0.0);
    assert_eq!(gaps::unmapped(&neutral).count(), 0);
    let mut gained = IspSettings::neutral(0.0);
    gained.wb = [1.5, 1.0, 1.2];
    assert_eq!(gaps::unmapped(&gained).collect::<Vec<_>>(), vec!["wb"]);
    assert!(GAPS.iter().any(|g| g.block.starts_with("WB")));
    assert_eq!(GAPS.len(), 9);
}

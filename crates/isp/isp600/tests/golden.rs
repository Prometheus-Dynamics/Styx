//! Goldens recorded from the Lemnos library's output for fixed inputs. `UPDATE_GOLDEN=1` rewrites
//! them from the library; a normal run compares against the committed files.

use std::fmt::Write as _;
use std::path::PathBuf;

use styx_isp600::job;
use styx_isp600::layout;
use styx_isp600::library::layout::{LOAD_REGIONS, STATISTICS_REGIONS};
use styx_isp600::load::LoadImage;

fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("read golden");
    assert_eq!(
        actual, expected,
        "golden {name} differs; rerun with UPDATE_GOLDEN=1 to re-record"
    );
}

/// FNV-1a 64, for a compact fingerprint of a whole image.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[test]
fn regions_match_the_recorded_library_tables() {
    let mut out = String::new();
    for (n, r) in LOAD_REGIONS.iter().enumerate() {
        writeln!(out, "load {n} {:#x} {:#x}", r.offset, r.bytes).unwrap();
    }
    for (n, r) in STATISTICS_REGIONS.iter().enumerate() {
        writeln!(out, "stat {n} {:#x} {:#x}", r.offset, r.bytes).unwrap();
    }
    golden("regions.golden", &out);
}

#[test]
fn a_fixed_load_image_matches_its_recorded_fingerprint() {
    let mut image = LoadImage::new();
    image.set_word(0x0, 0x1234_5678).unwrap();
    image.set_word(0x134, 0x0000_0003).unwrap();
    image.set_modules(1 << 24, true).unwrap();
    for n in 1..=4u8 {
        let r = LOAD_REGIONS[usize::from(n)];
        let table: Vec<u8> = (0..r.bytes)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(n))
            .collect();
        image.set_table(n, &table).unwrap();
    }
    assert!(image.checked().is_ok());
    let mut out = String::new();
    writeln!(out, "fnv1a {:#018x}", fnv1a(image.bytes())).unwrap();
    writeln!(
        out,
        "word0 {:#010x}",
        u32::from_le_bytes(image.bytes()[0..4].try_into().unwrap())
    )
    .unwrap();
    writeln!(
        out,
        "word134 {:#010x}",
        u32::from_le_bytes(image.bytes()[0x134..0x138].try_into().unwrap())
    )
    .unwrap();
    golden("load_image.golden", &out);
}

#[test]
fn a_fixed_job_encodes_to_its_recorded_bytes() {
    let j = job::job(0x0102_0304_0506_0708, 1, Some(2));
    let (bytes, used) = job::encode(&j).unwrap();
    let mut out = String::new();
    for b in &bytes[..used] {
        write!(out, "{b:02x}").unwrap();
    }
    out.push('\n');
    golden("job.golden", &out);
}

#[test]
fn the_contract_numbers_agree_with_the_library_tables() {
    let info = layout::info(0);
    assert_eq!(
        info.load_bytes as usize,
        styx_isp600::library::layout::LOAD_BYTES
    );
    for n in 0..LOAD_REGIONS.len() as u8 {
        let w = layout::load_region(n).unwrap();
        assert_eq!(w.offset as usize, LOAD_REGIONS[usize::from(n)].offset);
    }
}

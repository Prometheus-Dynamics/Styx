//! Goldens recorded from the Lemnos library's output for fixed inputs. `UPDATE_GOLDEN=1` rewrites
//! them from the library; a normal run compares against the committed files.

use std::fmt::Write as _;
use std::path::PathBuf;

use styx_isp600::job;
use styx_isp600::layout;
use styx_isp600::library::layout::{LOAD_REGIONS, STATISTICS_REGIONS};

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

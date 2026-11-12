//! What this crate says about each format against what `dev` said.
//!
//! `data/dev-golden.tsv` was written by `dev`'s own code (e74369a1, `styx-core-rs`):
//! `#` rows hold each code's facts (storage, compressed, JPEG, packed pixel, planes,
//! subsampling, bit depth, default colour space); the other rows hold `default_layouts_for_format`
//! for sizes and alignments (`stride/len` per plane, `-` where dev had no layout).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use styx_format::{FORMATS, FourCc, LayoutError, Storage, drm, plane_sizes};

const GOLDEN: &str = include_str!("data/dev-golden.tsv");

fn code(s: &str) -> FourCc {
    s.parse().unwrap_or_else(|_| panic!("bad code {s:?}"))
}

fn facts() -> impl Iterator<Item = Vec<&'static str>> {
    GOLDEN
        .lines()
        .filter_map(|l| l.strip_prefix("#\t"))
        .map(|l| l.split('\t').collect())
}

fn dev_codes() -> BTreeSet<FourCc> {
    facts().map(|f| code(f[0])).collect()
}

/// Dev's storage names, `Unknown` where dev had no layout for the code.
fn storage(name: &str) -> Option<Storage> {
    match name {
        "Packed" => Some(Storage::Packed),
        "SemiPlanar" => Some(Storage::SemiPlanar),
        "Planar" => Some(Storage::Planar),
        "RawBayer" => Some(Storage::RawBayer),
        "Compressed" => Some(Storage::Compressed),
        "Unknown" => None,
        other => panic!("storage {other}"),
    }
}

#[test]
fn every_dev_format_round_trips() {
    let codes = dev_codes();
    assert_eq!(codes.len(), 76);
    for c in codes {
        let text = c.to_string();
        assert_eq!(text.parse::<FourCc>(), Ok(c), "{c} through text");
        assert_eq!(FourCc::from(c.to_u32()), c, "{c} through its number");
        assert_eq!(FourCc::from(c.bytes()), c, "{c} through its bytes");
        let info = c.info().unwrap_or_else(|| panic!("{c} has no row"));
        assert_eq!(info.fourcc, c);
        #[cfg(feature = "serde")]
        {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(
                serde_json::from_str::<FourCc>(&json).unwrap(),
                c,
                "{c} through serde"
            );
        }
        match drm::to_drm(c) {
            Some(d) => {
                let back = drm::from_drm(d.fourcc, d.modifier).expect("maps back");
                assert_eq!(drm::to_drm(back), Some(d), "{c} through DRM");
            }
            None => assert!(
                drm::UNMAPPED.contains(&c),
                "{c} neither mapped nor unmapped"
            ),
        }
    }
}

#[test]
fn every_format_known_here_is_one_dev_knew() {
    let dev = dev_codes();
    for f in FORMATS {
        assert!(
            dev.contains(&f.fourcc),
            "{} is not one of dev's codes",
            f.fourcc
        );
    }
    assert_eq!(FORMATS.len(), dev.len());
}

#[test]
fn facts_match_dev_where_dev_had_them() {
    for f in facts() {
        let c = code(f[0]);
        let info = c.info().unwrap();
        let Some(storage) = storage(f[1]) else {
            continue;
        };
        assert_eq!(info.storage, storage, "{c} storage");
        assert_eq!(
            (info.storage == Storage::Compressed).to_string(),
            f[2],
            "{c} compressed"
        );
        assert_eq!(info.jpeg.to_string(), f[3], "{c} jpeg");
        let packed = info
            .packed
            .map(|p| format!("Some(({}, {:?}))", p.bytes_per_pixel, p.order))
            .unwrap_or_else(|| "None".into());
        assert_eq!(packed, f[4], "{c} packed pixel");
        assert_eq!(info.planes.to_string(), f[5], "{c} planes");
        assert_eq!(format!("{:?}", info.subsampling), f[6], "{c} subsampling");
        if f[7] != "Unknown" {
            assert_eq!(format!("{:?}", info.bit_depth), f[7], "{c} bit depth");
        }
        assert_eq!(format!("{:?}", info.default_color), f[8], "{c} colour");
    }
}

#[test]
fn plane_sizes_match_dev() {
    let mut matched = 0;
    let mut added = BTreeSet::new();
    for line in GOLDEN.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').collect();
        let c = code(f[0]);
        let n = |s: &str| s.parse::<u32>().unwrap();
        let align = |s: &str| Some(s.parse::<usize>().unwrap()).filter(|&a| a != 0);
        let ours = plane_sizes(c, n(f[1]), n(f[2]), align(f[3]), align(f[4]));
        if f[5] == "-" {
            match ours {
                Err(LayoutError::Compressed(_)) => assert!(c.is_compressed()),
                Ok(_) => {
                    added.insert(c);
                }
                Err(e) => panic!("{line}: {e}"),
            }
            continue;
        }
        let mut text = String::new();
        for (i, p) in ours
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .iter()
            .enumerate()
        {
            if i > 0 {
                text.push(' ');
            }
            write!(text, "{}/{}", p.stride, p.len).unwrap();
        }
        assert_eq!(text, f[5], "{line}");
        matched += 1;
    }
    assert_eq!(matched, 765, "rows where dev had a layout");
    // Formats dev knew the code of but laid out nothing for, which have planes here.
    let added: Vec<String> = added.iter().map(ToString::to_string).collect();
    println!("laid out here, not on dev: {}", added.join(" "));
    assert_eq!(added.len(), 20);
}

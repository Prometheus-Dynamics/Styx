//! The parameter buffers and the statistics decode match dev's (the Rockchip path on the dev
//! branch, run on the recorded settings of `algo/controller`'s replay).
//!
//! The goldens in `data/` are dev's output: `f000` and `f040` are frames 0 and 40 of the recording,
//! `syn` is the synthetic settings file (a lens shading grid, a tone curve, black channels) at
//! frame 7, each for ISP21 and ISP30. `dev-stats-seed1.txt` is dev's statistics decode of a
//! fixed pseudo-random buffer per generation. The full recording (80 frames) was compared the
//! same way when the goldens were made.

use styx_aaa::Statistics;
use styx_rkisp::{Generation, Params, statistics};
use styx_settings::IspSettings;

const RECORDING: &str = include_str!("../../../algo/controller/tests/data/dev-expected.jsonl");

fn golden(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn frame_settings(frame: u32) -> (IspSettings, u32) {
    for line in RECORDING.lines().skip(1) {
        let v: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
        if v["frame"].as_u64() == Some(u64::from(frame)) {
            let s = serde_json::from_value(v["isp"].clone()).expect("settings");
            return (s, frame);
        }
    }
    panic!("frame {frame} is not in the recording");
}

fn check(g: Generation, tag: &str, settings: &IspSettings, frame: u32) {
    let name = match g {
        Generation::Isp21 => "isp21",
        Generation::Isp30 => "isp30",
        Generation::Isp35 => unreachable!("ISP35 is not mapped"),
    };
    let mut p = Params::new(g);
    p.apply(settings, frame, 1280, 800).expect("apply");
    let want = golden(&format!("{tag}-{name}.bin"));
    assert_eq!(p.as_bytes().len(), want.len(), "{tag} {name} length");
    let first = p.as_bytes().iter().zip(&want).position(|(a, b)| a != b);
    assert_eq!(
        first, None,
        "{tag} {name}: first byte that differs from dev"
    );
}

#[test]
fn recorded_frames_match_devs_buffers() {
    for frame in [0, 40] {
        let (s, f) = frame_settings(frame);
        for g in [Generation::Isp21, Generation::Isp30] {
            check(g, &format!("f{frame:03}"), &s, f);
        }
    }
}

#[test]
fn the_synthetic_settings_match_devs_buffers() {
    let text = std::fs::read_to_string(format!(
        "{}/tests/data/synthetic-settings.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the synthetic settings");
    let s: IspSettings = serde_json::from_str(&text).expect("settings JSON");
    for g in [Generation::Isp21, Generation::Isp30] {
        check(g, "syn", &s, 7);
    }
}

/// The same pseudo-random buffer dev's dump used: a 64-bit LCG's high byte.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 56) as u8
        })
        .collect()
}

#[test]
fn statistics_decode_like_devs() {
    let mut text = String::new();
    for (g, name, len) in [
        (Generation::Isp21, "isp21", 13700usize),
        (Generation::Isp30, "isp30", 16172),
    ] {
        let buf = noise(len, 77 + len as u64);
        let mut st = Statistics::default();
        statistics(g, &buf, 1280 * 800 / 225, &mut st).expect("decode");
        text.push_str(&format!("{name} seed 1\n"));
        text.push_str(&format!(
            "colour {}x{} {}\n",
            st.colour.width,
            st.colour.height,
            st.colour.zones.len()
        ));
        for z in &st.colour.zones {
            text.push_str(&format!(
                "{:x} {:x} {:x} {}\n",
                z.r.to_bits(),
                z.g.to_bits(),
                z.b.to_bits(),
                z.counted
            ));
        }
        text.push_str("bins");
        for b in st.histogram.bins() {
            text.push_str(&format!(" {b}"));
        }
        text.push('\n');
        text.push_str(&format!(
            "before_wb {} before_lsc {} luma {}\n",
            st.before_wb,
            st.before_lsc,
            st.luma.is_some()
        ));
    }
    let want = String::from_utf8(golden("dev-stats-seed1.txt")).expect("UTF-8 golden");
    assert_eq!(text, want);
}

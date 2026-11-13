//! The K230 writes against dev's, and the layout against Lemnos's checks.
//!
//! The goldens in `tests/data` are dev's writes (dev's `styx_pipeline::k230isp::params`, run on
//! the settings of `crates/algo/controller/tests/data/dev-expected.jsonl`), dumped from a copy of
//! dev. `dev-synthetic-nolsc` is the same mapping on the synthetic settings of `synthetic` below,
//! without lens shading (Styx does not write lens shading; see the crate docs).

use lemnos_core::{Decode, Encode, Reader, Writer};
use styx_lemnos::k230isp::config::{self, MAX_WRITES};
use styx_lemnos::k230isp::regs as r;
use styx_settings::IspSettings;
use styx_soft::CfaPattern;

use crate::params::{ConfigError, Params, RegWrite};
use crate::settings;

const DEV_EXPECTED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../algo/controller/tests/data/dev-expected.jsonl"
);
const GOLDEN_FRAMES: [(usize, &str); 3] = [
    (1, include_str!("../tests/data/dev-frame1.txt")),
    (41, include_str!("../tests/data/dev-frame41.txt")),
    (80, include_str!("../tests/data/dev-frame80.txt")),
];
const GOLDEN_SYNTHETIC: &str = include_str!("../tests/data/dev-synthetic-nolsc.txt");

const SIZE: (u32, u32) = (1280, 800);

/// Dev's dump: the count, the `isp_ctrl` bits, then one `addr value` pair per line.
fn parse_golden(text: &str) -> (Vec<RegWrite>, u32) {
    let mut lines = text.lines();
    let count: usize = lines
        .next()
        .and_then(|l| l.strip_prefix("count "))
        .and_then(|n| n.parse().ok())
        .expect("a count line");
    let ctrl = lines
        .next()
        .and_then(|l| l.strip_prefix("ctrl_bits "))
        .and_then(hex)
        .expect("a ctrl_bits line");
    let writes: Vec<RegWrite> = lines
        .map(|l| {
            let mut it = l.split_whitespace();
            RegWrite {
                addr: it.next().and_then(hex).expect("an address"),
                value: it.next().and_then(hex).expect("a value"),
            }
        })
        .collect();
    assert_eq!(writes.len(), count, "the golden's count");
    (writes, ctrl)
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

/// The settings dev's controller produced for frame `index` of the recording (line `index` of
/// the expected output: line 0 is the start).
fn dev_settings(index: usize) -> IspSettings {
    let text = std::fs::read_to_string(DEV_EXPECTED).expect("dev's expected output");
    let line = text.lines().nth(index).expect("a line");
    let value: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
    serde_json::from_value(value["isp"].clone()).expect("settings")
}

/// The same settings as the synthetic golden (dev's `dump` test in its copy).
fn synthetic() -> IspSettings {
    let mut s = IspSettings::neutral(64.0 / 1024.0);
    s.black_channels = Some([0.02, 0.03, 0.04, 0.05]);
    s.wb = [2.0, 1.0, 1.5];
    s.digital_gain = 1.25;
    s.ccm = [1.5, -0.25, -0.25, -0.5, 1.75, -0.25, 0.125, -0.5, 1.375];
    s
}

/// The writes as dev's `apply` lays them out: the list, then `isp_ctrl` (set last).
fn expected(writes: Vec<RegWrite>, ctrl: u32) -> Vec<RegWrite> {
    let mut all = writes;
    if ctrl != 0 {
        all.push(RegWrite {
            addr: r::ISP_CTRL,
            value: ctrl,
        });
    }
    all
}

fn encoded(p: &Params) -> Vec<u8> {
    let mut buf = vec![0u8; p.encoded_len().expect("a layout")];
    let n = p.encode(&mut buf).expect("encodes");
    assert_eq!(n, buf.len());
    buf
}

fn decode(bytes: &[u8]) -> Vec<RegWrite> {
    config::check(bytes)
        .expect("Lemnos's check accepts the configuration")
        .map(|(addr, value)| RegWrite { addr, value })
        .collect()
}

#[test]
fn dev_frames_give_the_same_writes() {
    for (index, golden) in GOLDEN_FRAMES {
        let (want, ctrl) = parse_golden(golden);
        let p = settings::params(Params::new(), &dev_settings(index), CfaPattern::Rggb, SIZE);
        assert_eq!(p.writes(), &want[..], "frame at line {index}");
        assert_eq!(p.ctrl_bits(), ctrl, "frame at line {index}: isp_ctrl");
    }
}

#[test]
fn dev_synthetic_gives_the_same_bytes_in_lemnos_layout() {
    let (want, ctrl) = parse_golden(GOLDEN_SYNTHETIC);
    let p = settings::params(Params::new(), &synthetic(), CfaPattern::Bggr, SIZE);
    assert_eq!(p.writes(), &want[..]);
    assert_eq!(p.ctrl_bits(), ctrl);

    // Byte for byte: count, then each (addr, value) little-endian, isp_ctrl last.
    let bytes = encoded(&p);
    let all = expected(want, ctrl);
    let mut want_bytes = (all.len() as u32).to_le_bytes().to_vec();
    for w in all {
        want_bytes.extend_from_slice(&w.addr.to_le_bytes());
        want_bytes.extend_from_slice(&w.value.to_le_bytes());
    }
    assert_eq!(bytes, want_bytes);
}

#[test]
fn every_golden_passes_lemnos_check() {
    for (index, golden) in GOLDEN_FRAMES {
        let (want, ctrl) = parse_golden(golden);
        let p = settings::params(Params::new(), &dev_settings(index), CfaPattern::Rggb, SIZE);
        assert_eq!(decode(&encoded(&p)), expected(want, ctrl));
    }
}

#[test]
fn lens_shading_is_not_written() {
    let mut with = synthetic();
    with.lens_shading = Some(
        serde_json::from_value(serde_json::json!({
            "width": 4, "height": 3,
            "r": vec![1.0; 12], "g": vec![1.1; 12], "b": vec![1.2; 12],
        }))
        .expect("a lens shading table"),
    );
    let a = settings::params(Params::new(), &with, CfaPattern::Bggr, SIZE);
    let b = settings::params(Params::new(), &synthetic(), CfaPattern::Bggr, SIZE);
    assert_eq!(a, b);
    assert!(
        a.writes()
            .iter()
            .all(|w| !(0x2200..0x2268).contains(&w.addr))
    );
}

#[test]
fn rebuilt_list_has_the_same_bytes() {
    let s = synthetic();
    let first = settings::params(Params::new(), &s, CfaPattern::Bggr, SIZE);
    let again = settings::params(first.clone(), &s, CfaPattern::Bggr, SIZE);
    assert_eq!(encoded(&first), encoded(&again));
}

#[test]
fn white_balance_clamps_to_the_field() {
    let p = Params::new().white_balance([0.0, 5.0, -1.0]);
    let writes = p.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(
        writes[0],
        RegWrite {
            addr: r::ISP_AWB_GAIN_G,
            value: (1023 << 16) | 1023
        }
    );
    assert_eq!(
        writes[1],
        RegWrite {
            addr: r::ISP_AWB_GAIN_RB,
            value: (1 << 16) | 1
        }
    );
}

#[test]
fn too_many_writes_are_refused() {
    let mut p = Params::new();
    for _ in 0..70 {
        p = p.gamma(|x| x);
    }
    assert!(p.writes().len() > MAX_WRITES);
    assert!(matches!(
        p.encoded_len(),
        Err(ConfigError::TooManyWrites(_))
    ));
    let mut small = [0u8; 8];
    assert!(matches!(
        Params::new()
            .white_balance([1.0; 3])
            .encode(&mut small[..4]),
        Err(ConfigError::TooSmall(_))
    ));
}

#[test]
fn job_round_trips_through_lemnos_codec() {
    use lemnos_contract_camera_isp::{IspJob, MAX_OUTPUTS, NO_BUFFER};
    let job = IspJob {
        identity: 7,
        layout: styx_lemnos::k230isp::LAYOUT_WRITES,
        config_buffer: 3,
        input_buffer: NO_BUFFER,
        outputs: [NO_BUFFER; MAX_OUTPUTS],
    };
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    job.encode(&mut w).expect("encodes");
    let n = w.len();
    let back = IspJob::decode(&mut Reader::new(&buf[..n])).expect("decodes");
    assert_eq!(back, job);
}

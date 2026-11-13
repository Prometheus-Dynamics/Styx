//! Output and statistics equal to dev's software ISP, bit for bit, on the recorded raw frame.

mod common;

use common::*;
use styx_aaa::Statistics;
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

/// The picture kinds the goldens cover.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Rgb,
    Nv12,
    I420,
    Luma,
}

impl Kind {
    fn code(self) -> FourCc {
        match self {
            Kind::Rgb => FourCc::RGB3,
            Kind::Nv12 => FourCc::NV12,
            Kind::I420 => FourCc::I420,
            Kind::Luma => FourCc::GREY,
        }
    }

    /// (row bytes, rows) of each plane at `w` x `h`.
    fn planes(self, w: usize, h: usize) -> Vec<(usize, usize)> {
        match self {
            Kind::Rgb => vec![(w * 3, h)],
            Kind::Luma => vec![(w, h)],
            Kind::Nv12 => vec![(w, h), (w, h / 2)],
            Kind::I420 => vec![(w, h), (w / 2, h / 2), (w / 2, h / 2)],
        }
    }
}

struct Case {
    name: &'static str,
    kind: Kind,
    half: bool,
    demosaic: Demosaic,
    yuv: YuvMatrix,
    shaded: bool,
    tone: bool,
    stats: bool,
}

const CASES: [Case; 5] = [
    Case {
        name: "rgb_full",
        kind: Kind::Rgb,
        half: false,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: true,
        stats: true,
    },
    Case {
        name: "nv12_full",
        kind: Kind::Nv12,
        half: false,
        demosaic: Demosaic::Mhc,
        yuv: YuvMatrix::Bt709Limited,
        shaded: false,
        tone: true,
        stats: true,
    },
    Case {
        name: "i420_half",
        kind: Kind::I420,
        half: true,
        demosaic: Demosaic::EdgeDirected,
        yuv: YuvMatrix::Bt601Full,
        shaded: true,
        tone: true,
        stats: true,
    },
    Case {
        name: "luma_full",
        kind: Kind::Luma,
        half: false,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: false,
        stats: true,
    },
    Case {
        name: "rgb_half_nostats",
        kind: Kind::Rgb,
        half: true,
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        shaded: true,
        tone: true,
        stats: false,
    },
];

/// The bytes of a picture's planes, row by row, in plane order (as the golden stores them).
fn packed(lease: &FrameLease<()>, kind: Kind, w: usize, h: usize) -> Vec<u8> {
    let read = lease.read().unwrap();
    let mut out = Vec::new();
    for (plane, (row_bytes, rows)) in kind.planes(w, h).into_iter().enumerate() {
        let plane = read.plane(plane).unwrap();
        for row in 0..rows {
            out.extend_from_slice(plane.row_bytes(row, row_bytes).unwrap());
        }
    }
    out
}

/// dev's statistics of a case, from its `.stats` file.
struct GoldenStats {
    zones_x: u32,
    zones_y: u32,
    gains: [f64; 3],
    zones: Vec<[u64; 4]>,
    histogram: Vec<u64>,
}

fn golden_stats(name: &str) -> GoldenStats {
    let text = String::from_utf8(data(&format!("{name}.stats"))).unwrap();
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    let gains = [head[6], head[7], head[8]]
        .map(|hex| f32::from_bits(u32::from_str_radix(hex, 16).unwrap()) as f64);
    let (zones_x, zones_y) = (head[1].parse().unwrap(), head[2].parse().unwrap());
    let mut zones = Vec::new();
    let mut histogram = Vec::new();
    for line in lines {
        if let Some(bins) = line.strip_prefix("hist ") {
            histogram = bins
                .split_whitespace()
                .map(|v| v.parse().unwrap())
                .collect();
        } else {
            let v: Vec<u64> = line
                .split_whitespace()
                .map(|v| v.parse().unwrap())
                .collect();
            zones.push([v[0], v[1], v[2], v[3]]);
        }
    }
    GoldenStats {
        zones_x,
        zones_y,
        gains,
        zones,
        histogram,
    }
}

#[test]
fn outputs_and_statistics_equal_dev_bit_for_bit() {
    let format = raw_format();
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    for case in CASES {
        let (w, h) = if case.half { (W / 2, H / 2) } else { (W, H) };
        let options = options(case.demosaic, case.yuv, case.stats);
        let mut stage =
            SoftIspStage::new(RAW, W, H, &settings(case.shaded, case.tone), options).unwrap();
        assert_eq!(stage.format(), format);

        let out_pool = picture_pool(case.kind.code(), w, h, 1);
        let mut out = picture(&out_pool, case.kind.code(), w, h, 2);
        let mut stats = Statistics::default();
        stage
            .process(&raw, &mut out, case.stats.then_some(&mut stats))
            .unwrap_or_else(|e| panic!("{}: {e}", case.name));

        let want = data(&format!("{}.out", case.name));
        let got = packed(&out, case.kind, w as usize, h as usize);
        assert!(got == want, "{}: picture differs from dev's", case.name);

        if case.stats {
            check_stats(case.name, &stats);
        } else {
            assert!(
                stats.colour.zones.is_empty(),
                "{}: no stats asked",
                case.name
            );
        }
    }
}

fn check_stats(name: &str, stats: &Statistics) {
    let want = golden_stats(name);
    assert_eq!(
        (stats.colour.width, stats.colour.height),
        (want.zones_x, want.zones_y),
        "{name}"
    );
    assert_eq!(stats.colour.zones.len(), want.zones.len(), "{name}");
    for (i, (got, [r, g, b, count])) in stats.colour.zones.iter().zip(&want.zones).enumerate() {
        // The colour sums are dev's sums divided by the white balance gains: back to dev's.
        assert_eq!(
            (got.r * want.gains[0]).round() as u64,
            *r,
            "{name}: zone {i} red"
        );
        assert_eq!(
            (got.g * want.gains[1]).round() as u64,
            *g,
            "{name}: zone {i} green"
        );
        assert_eq!(
            (got.b * want.gains[2]).round() as u64,
            *b,
            "{name}: zone {i} blue"
        );
        assert_eq!(got.counted as u64, *count, "{name}: zone {i} count");
    }
    assert_eq!(
        stats.histogram.bins(),
        want.histogram.as_slice(),
        "{name}: histogram"
    );
    assert!(stats.luma.is_none() && stats.focus.is_none());
    assert!(stats.before_wb && !stats.before_lsc);
}

/// `Arithmetic::ApproxTone` (the tone curve as quadratics, the approximation) is dev's AVX2
/// picture of the luma case, bit for bit; `Arithmetic::Auto` (the exact table, on every host)
/// differs from it by one code on some samples, and nowhere by more. Only the luma case has
/// samples where they differ: its default sRGB curve is the one whose quadratics are farthest
/// from the table (the others match exactly).
#[test]
fn approx_tone_is_the_named_approximation_within_one_code() {
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    let picture_of = |arithmetic: styx_soft::params::Arithmetic| {
        let mut options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, false);
        options.arithmetic = arithmetic;
        let mut stage = SoftIspStage::new(RAW, W, H, &settings(true, false), options).unwrap();
        let out_pool = picture_pool(FourCc::GREY, W, H, 1);
        let mut out = picture(&out_pool, FourCc::GREY, W, H, 2);
        stage.process(&raw, &mut out, None).unwrap();
        packed(&out, Kind::Luma, W as usize, H as usize)
    };
    let auto = picture_of(styx_soft::params::Arithmetic::Auto);
    let approx = picture_of(styx_soft::params::Arithmetic::ApproxTone);

    assert!(
        approx == data("luma_full.approx.out"),
        "ApproxTone: dev's picture"
    );
    assert!(auto == data("luma_full.out"), "Auto: the exact table");
    let differing: Vec<u8> = auto
        .iter()
        .zip(&approx)
        .map(|(a, b)| a.abs_diff(*b))
        .filter(|&d| d != 0)
        .collect();
    assert_eq!(
        differing.len(),
        372,
        "samples where Auto and ApproxTone differ"
    );
    assert!(differing.iter().all(|&d| d == 1), "by one code at most");
}

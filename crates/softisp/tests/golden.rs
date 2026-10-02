//! Output pinned bit for bit: a textured frame through every output, scale, demosaic and
//! thread count, hashed and compared with the hashes the ISP produced before its kernels were
//! fused and threaded (styx-softisp 2.0.0 as of the native-stack branch). Any change of the
//! output fails here; a deliberate quality trade-off must be opt-in and leave these alone.
//!
//! `STYX_GOLDEN_PRINT=1 cargo test -p styx-softisp --test golden -- --nocapture` prints the
//! hashes instead of checking them.

mod common;

use common::{mosaic, pack};
use styx_softisp::*;

const W: usize = 264;
const H: usize = 100;

/// Gradients, fine stripes, a hard edge, clipped highlights and noise, 10-bit.
fn frame(pattern: CfaPattern) -> (Vec<u8>, usize) {
    let m = mosaic(W, H, pattern, |x, y| {
        let mut seed = (x as u32 * 7919 + y as u32 * 104_729) ^ 0x2545_F491;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = (seed & 31) as i32 - 16;
        let base = [
            (x * 1023 / W) as i32,
            ((x + y) * 7 % 900) as i32 + if (x / 3) % 2 == 0 { 60 } else { 0 },
            if x > W / 2 { 900 } else { 120 + (y * 9) as i32 },
        ];
        let clip = if (100..120).contains(&x) && y < 30 {
            1023
        } else {
            0
        };
        base.map(|v| (v + noise).max(clip).clamp(0, 1023) as u16)
    });
    pack(&m, W, H, RawPacking::Csi2Raw10)
}

fn params(demosaic: Demosaic, shaded: bool, stats: bool) -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.9,
            g: 1.0,
            b: 1.6,
        }),
        digital_gain: 1.3,
        lens_shading: shaded.then(|| LensShading::radial(9, 7, 0.7)),
        demosaic,
        ccm: Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        }),
        tone: Some(ToneCurve::Points {
            points: vec![
                [0.0, 0.0],
                [0.05, 0.2],
                [0.25, 0.55],
                [0.6, 0.85],
                [1.0, 1.0],
            ],
        }),
        yuv: YuvMatrix::Bt601Full,
        stats: stats.then_some(StatsConfig {
            zones_x: 8,
            zones_y: 6,
            histogram_bins: 64,
            saturation: 0.95,
            row_step: 1,
        }),
    }
}

/// FNV-1a over bytes.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

/// Hash of every output of one configuration (RGB24, NV12, I420, luma) and its statistics.
fn run(isp: &mut SoftIsp, raw: &[u8], stride: usize, scale: Scale) -> u64 {
    let (w, h) = isp.output_size(scale);
    let (w, h) = (w as usize, h as usize);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let stats_hash = |h: &mut u64, s: Option<IspStats>| {
        if let Some(s) = s {
            fnv(h, serde_json_like(&s).as_bytes());
        }
    };
    let mut rgb = vec![0u8; w * h * 3];
    let s = isp
        .process(
            raw,
            stride,
            scale,
            OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: w * 3,
            },
        )
        .unwrap();
    fnv(&mut hash, &rgb);
    stats_hash(&mut hash, s);
    let (mut y, mut uv) = (vec![0u8; w * h], vec![0u8; w * h / 2]);
    isp.process(
        raw,
        stride,
        scale,
        OutputBuffers::Nv12 {
            y: &mut y,
            y_stride: w,
            uv: &mut uv,
            uv_stride: w,
        },
    )
    .unwrap();
    fnv(&mut hash, &y);
    fnv(&mut hash, &uv);
    let (mut u, mut v) = (vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
    isp.process(
        raw,
        stride,
        scale,
        OutputBuffers::I420 {
            y: &mut y,
            y_stride: w,
            u: &mut u,
            u_stride: w / 2,
            v: &mut v,
            v_stride: w / 2,
        },
    )
    .unwrap();
    fnv(&mut hash, &y);
    fnv(&mut hash, &u);
    fnv(&mut hash, &v);
    let s = isp
        .process(
            raw,
            stride,
            scale,
            OutputBuffers::Luma {
                data: &mut y,
                stride: w,
            },
        )
        .unwrap();
    fnv(&mut hash, &y);
    stats_hash(&mut hash, s);
    hash
}

/// The statistics as text (exact for the integer sums; zone luma to 6 decimals).
fn serde_json_like(s: &IspStats) -> String {
    let mut t = format!(
        "{}x{} {} {:?};",
        s.zones_x, s.zones_y, s.samples, s.histogram
    );
    for z in &s.zones {
        t += &format!(
            "{} {} {} {} {:.6};",
            z.r_sum, z.g_sum, z.b_sum, z.count, z.luma
        );
    }
    t
}

/// (pattern, demosaic, lens shading, statistics, scale) and the hash the reference produced.
const CASES: [(CfaPattern, Demosaic, bool, bool, Scale, u64); 8] = [
    (
        CfaPattern::Bggr,
        Demosaic::Bilinear,
        true,
        true,
        Scale::Full,
        0x764250AD6A8258D4,
    ),
    (
        CfaPattern::Bggr,
        Demosaic::Bilinear,
        false,
        false,
        Scale::Full,
        0xDB26345C7AC16947,
    ),
    (
        CfaPattern::Rggb,
        Demosaic::Mhc,
        true,
        true,
        Scale::Full,
        0x2A2C6119E5621964,
    ),
    (
        CfaPattern::Gbrg,
        Demosaic::Mhc,
        false,
        false,
        Scale::Full,
        0x8945224EC1C232A1,
    ),
    (
        CfaPattern::Grbg,
        Demosaic::Bilinear,
        true,
        true,
        Scale::Full,
        0xC5DE7D5E63E1BDFD,
    ),
    (
        CfaPattern::Bggr,
        Demosaic::Bilinear,
        true,
        true,
        Scale::Half,
        0x13ECF8138E5EFED3,
    ),
    (
        CfaPattern::Rggb,
        Demosaic::Bilinear,
        false,
        false,
        Scale::Half,
        0xCA7D345553F804CF,
    ),
    (
        CfaPattern::Grbg,
        Demosaic::Mhc,
        true,
        true,
        Scale::Half,
        0xA25DE1A9A4904479,
    ),
];

#[test]
fn output_is_pinned_bit_for_bit() {
    let print = std::env::var_os("STYX_GOLDEN_PRINT").is_some();
    for (pattern, demosaic, shaded, stats, scale, want) in CASES {
        let (raw, stride) = frame(pattern);
        let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
        for (threads, copy) in [(1, true), (3, true), (1, false)] {
            let mut isp = SoftIsp::new(format, params(demosaic, shaded, stats))
                .unwrap()
                .with_threads(threads)
                .with_copy_input(copy);
            let got = run(&mut isp, &raw, stride, scale);
            let case = format!("{pattern:?} {demosaic:?} shaded {shaded} stats {stats} {scale:?}");
            if print {
                println!("{case} threads {threads} copy {copy}: {got:#018x}");
            } else {
                assert_eq!(got, want, "{case}, {threads} threads, copy {copy}");
            }
        }
    }
}

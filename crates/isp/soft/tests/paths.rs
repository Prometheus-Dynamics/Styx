//! The row paths of [`styx_soft::Paths`]: each new path (`Paths::CURRENT`) equals the path it
//! replaces (`Paths::LEGACY`), bit for bit, on random raw frames and every geometry the
//! pipeline's edges and windows meet. The settings cover the neutral case (identity matrix, sRGB
//! tone), the 8-bit direct luma's eligible and ineligible set-ups, and lens shading.

use styx_aaa::LensShading;
use styx_settings::IspSettings;
use styx_soft::params::{IspParams, Paths};
use styx_soft::settings::{Options, params_for};
use styx_soft::{CfaPattern, OutputBuffers, RawFormat, RawPacking, Scale, SoftIsp, Window};

/// Deterministic random bytes (xorshift).
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Luma,
    Rgb,
    Nv12,
}

/// Processes `input` (stride `stride`) with `params` into `kind` at `scale`, or the window.
fn run(
    format: RawFormat,
    params: &IspParams,
    input: &[u8],
    stride: usize,
    scale: Scale,
    kind: Kind,
    window: Option<Window>,
) -> Vec<Vec<u8>> {
    let mut isp = SoftIsp::new(format, params.clone()).expect("the parameters are valid");
    let (w, h) = match window {
        Some(win) => isp.window_size(win, scale),
        None => isp.output_size(scale),
    };
    let (w, h) = (w as usize, h as usize);
    let process = |isp: &mut SoftIsp, out: OutputBuffers<'_>| match window {
        Some(win) => isp.process_window(input, stride, win, scale, out).unwrap(),
        None => drop(isp.process(input, stride, scale, out).unwrap()),
    };
    match kind {
        Kind::Luma => {
            let mut y = vec![0; w * h];
            process(
                &mut isp,
                OutputBuffers::Luma {
                    data: &mut y,
                    stride: w,
                },
            );
            vec![y]
        }
        Kind::Rgb => {
            let mut rgb = vec![0; 3 * w * h];
            process(
                &mut isp,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: 3 * w,
                },
            );
            vec![rgb]
        }
        Kind::Nv12 => {
            let mut y = vec![0; w * h];
            let mut uv = vec![0; w * h / 2];
            process(
                &mut isp,
                OutputBuffers::Nv12 {
                    y: &mut y,
                    y_stride: w,
                    uv: &mut uv,
                    uv_stride: w,
                },
            );
            vec![y, uv]
        }
    }
}

/// The parameters of `settings` on `format` with `paths`.
fn params(format: &RawFormat, settings: &IspSettings, paths: Paths) -> IspParams {
    let mut p = params_for(format, settings, &Options::default());
    p.paths = paths;
    p
}

fn neutral() -> IspSettings {
    IspSettings::neutral(0.0)
}

/// Every setting set the paths must agree on: neutral, black level, white balance, a colour
/// matrix (not the identity), and lens shading.
fn settings_cases() -> Vec<(&'static str, IspSettings)> {
    let mut ccm = neutral();
    ccm.ccm = [1.2, -0.1, -0.1, -0.2, 1.1, -0.1, 0.0, -0.3, 1.3];
    let mut black = neutral();
    black.black_level = 2.5 / 255.0;
    let mut wb = neutral();
    wb.wb = [1.2, 1.0, 1.1];
    let mut shaded = neutral();
    let grid = vec![1.0, 1.2, 1.0, 1.1, 1.3, 1.0];
    shaded.lens_shading = Some(LensShading {
        width: 3,
        height: 2,
        r: grid.clone(),
        g: grid.clone(),
        b: grid,
        generation: 0,
    });
    vec![
        ("neutral", neutral()),
        ("ccm", ccm),
        ("black", black),
        ("wb", wb),
        ("shaded", shaded),
    ]
}

/// Geometries: the smallest frames, odd-looking ones at the front end's group sizes, and a
/// frame wider than a few vectors.
const SIZES: [(u32, u32); 6] = [(4, 4), (8, 6), (16, 8), (20, 12), (64, 48), (130, 34)];

#[test]
fn current_paths_equal_legacy_on_every_geometry_and_setting() {
    for (name, settings) in settings_cases() {
        for pattern in [
            CfaPattern::Rggb,
            CfaPattern::Bggr,
            CfaPattern::Grbg,
            CfaPattern::Gbrg,
        ] {
            for (k, &(w, h)) in SIZES.iter().enumerate() {
                let format = RawFormat::new(w, h, pattern, RawPacking::U8);
                let input = bytes((w * h) as usize, k as u64 * 17 + pattern as u64);
                let legacy = params(&format, &settings, Paths::LEGACY);
                let current = params(&format, &settings, Paths::CURRENT);
                for kind in [Kind::Luma, Kind::Rgb, Kind::Nv12] {
                    for scale in [Scale::Full, Scale::Half] {
                        // 4:2:0 pictures need even sizes.
                        let (ow, oh) = match scale {
                            Scale::Full => (w, h),
                            Scale::Half => (w / 2, h / 2),
                        };
                        if matches!(kind, Kind::Nv12) && (ow % 2 == 1 || oh % 2 == 1) {
                            continue;
                        }
                        let want = run(format, &legacy, &input, w as usize, scale, kind, None);
                        let got = run(format, &current, &input, w as usize, scale, kind, None);
                        assert_eq!(got, want, "{name}: {pattern:?} {w}x{h} {kind:?} {scale:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn current_paths_equal_legacy_on_windows_and_at_16_bits() {
    let settings = neutral();
    let (w, h) = (64u32, 48u32);
    let format = RawFormat::new(w, h, CfaPattern::Grbg, RawPacking::U8);
    let input = bytes((w * h) as usize, 99);
    let legacy = params(&format, &settings, Paths::LEGACY);
    let current = params(&format, &settings, Paths::CURRENT);
    // Windows at the edges, inside, and touching the bottom right corner, each size
    // within the frame's even origins.
    for win in [
        Window::new(0, 0, 16, 8),
        Window::new(2, 4, 14, 10),
        Window::new(48, 36, 16, 12),
        Window::new(10, 2, 8, 6),
    ] {
        for scale in [Scale::Full, Scale::Half] {
            let want = run(
                format,
                &legacy,
                &input,
                w as usize,
                scale,
                Kind::Luma,
                Some(win),
            );
            let got = run(
                format,
                &current,
                &input,
                w as usize,
                scale,
                Kind::Luma,
                Some(win),
            );
            assert_eq!(got, want, "window {win:?} {scale:?}");
        }
    }
    // Ten-bit samples never take the direct path, and must still agree.
    let f10 = RawFormat::new(w, h, CfaPattern::Rggb, RawPacking::U16Le { bits: 10 });
    let input10 = bytes((w * h * 2) as usize, 7);
    let l10 = params(&f10, &settings, Paths::LEGACY);
    let c10 = params(&f10, &settings, Paths::CURRENT);
    for scale in [Scale::Full, Scale::Half] {
        let want = run(
            f10,
            &l10,
            &input10,
            (w * 2) as usize,
            scale,
            Kind::Luma,
            None,
        );
        let got = run(
            f10,
            &c10,
            &input10,
            (w * 2) as usize,
            scale,
            Kind::Luma,
            None,
        );
        assert_eq!(got, want, "10-bit {scale:?}");
    }
}

#[test]
fn flags_parse_by_name() {
    assert_eq!(Paths::parse("current").unwrap(), Paths::CURRENT);
    assert_eq!(Paths::parse("legacy").unwrap(), Paths::LEGACY);
    let p = Paths::parse("current,-direct-u8").unwrap();
    assert!(p.skip_noops && !p.direct_u8);
    let p = Paths::parse("legacy,+skip-noops").unwrap();
    assert!(p.skip_noops && !p.direct_u8);
    assert!(Paths::parse("fast").is_err());
    assert!(Paths::parse("current,direct-u8").is_err());
}

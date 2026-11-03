use super::*;

const PIXELS: [Pixels; 4] = [Pixels::Grey8, Pixels::Rgb888, Pixels::Yuv420p, Pixels::Nv12];

fn prepares(op: &mut BeOp) -> u32 {
    op.backend.prepare().expect("prepare");
    op.backend.config().global.rgb_enables
}

#[test]
fn capabilities_are_distinct() {
    for (i, a) in CAPABILITIES.iter().enumerate() {
        assert!(a.starts_with("styx.pisp."));
        assert!(!CAPABILITIES[i + 1..].contains(a), "{a}");
    }
}

#[test]
fn resample_prepares_on_output_0() {
    for p in PIXELS {
        for out in [(320, 240), (1280, 960)] {
            let mut op = resample(p, (640, 480), out, &defaults::LANCZOS3).unwrap();
            let en = prepares(&mut op);
            assert_eq!(op.output, 0);
            assert_ne!(en & rgb_enable::RESAMPLE0, 0, "{p:?}");
            assert_ne!(en & rgb_enable::OUTPUT0, 0);
            assert_eq!(en & rgb_enable::OUTPUT1, 0);
            assert_eq!(op.out.width, out.0);
        }
    }
    assert_eq!(
        resample(Pixels::Grey8, (8, 8), (16, 16), &defaults::LANCZOS3).unwrap_err(),
        OpError::TooSmall(8, 8)
    );
}

#[test]
fn downscale_prepares_on_output_1() {
    for p in PIXELS {
        for k in [2, 4, 8] {
            let mut op = downscale(p, (640, 480), k).unwrap();
            let en = prepares(&mut op);
            assert_eq!(op.output, 1);
            assert_ne!(en & rgb_enable::DOWNSCALE1, 0, "{p:?} {k}");
            assert_ne!(en & rgb_enable::OUTPUT1, 0);
            assert_eq!((op.out.width, op.out.height), (640 / k, 480 / k));
        }
    }
    for k in [1, 3, 9] {
        assert_eq!(
            downscale(Pixels::Grey8, (640, 480), k).unwrap_err(),
            OpError::Factor(k)
        );
    }
}

#[test]
fn grey_runs_the_mono_bayer_path_into_luma() {
    let op = downscale(Pixels::Grey8, (64, 64), 2).unwrap();
    let g = op.backend.config().global;
    assert_eq!(g.bayer_order, BayerOrder::Greyscale as u8);
    assert_eq!(g.bayer_enables, bayer_enable::INPUT);
    assert_ne!(g.rgb_enables & rgb_enable::YCBCR, 0);
    assert_eq!(g.rgb_enables & rgb_enable::INPUT, 0);
    // The JPEG luma row sums to exactly 1.0, so luma is the input.
    let y = &op.backend.config().ycbcr.coeffs[..3];
    assert_eq!(y.iter().map(|&c| i32::from(c)).sum::<i32>(), 1024);
    let op = downscale(Pixels::Nv12, (64, 64), 2).unwrap();
    assert_eq!(op.backend.config().global.bayer_enables, 0);
}

#[test]
fn a_linear_lut_is_exact() {
    let identity: [u8; 256] = core::array::from_fn(|i| i as u8);
    let (mut op, fit) = lut(Pixels::Grey8, (64, 64), &identity).unwrap();
    assert_eq!(
        fit,
        LutFit {
            inexact_codes: 0,
            max_error: 0
        }
    );
    assert_ne!(prepares(&mut op) & rgb_enable::GAMMA, 0);
    let invert_half: [u8; 256] = core::array::from_fn(|i| (i / 2) as u8);
    assert_eq!(
        lut(Pixels::Nv12, (64, 64), &invert_half)
            .unwrap()
            .1
            .max_error,
        0
    );
}

#[test]
fn a_threshold_lut_misses_around_its_step() {
    let step: [u8; 256] = core::array::from_fn(|i| if i < 100 { 0 } else { 255 });
    let fit = lut(Pixels::Grey8, (64, 64), &step).unwrap().1;
    // The jump climbs 63 levels a segment: it spreads over the segments after the step.
    assert!(fit.inexact_codes > 0 && fit.inexact_codes < 32, "{fit:?}");
    assert!(fit.max_error > 0);
    let falling: [u8; 256] = core::array::from_fn(|i| 255 - i as u8);
    assert_eq!(
        lut(Pixels::Grey8, (64, 64), &falling).unwrap_err(),
        OpError::Decreasing
    );
}

#[test]
fn filter5x5_sets_kernel_0_alone() {
    let mut k = [0i8; 25];
    k[12] = 24;
    k.iter_mut().filter(|v| **v == 0).for_each(|v| *v = -1);
    for p in [Pixels::Grey8, Pixels::Yuv420p, Pixels::Nv12] {
        let mut op = filter5x5(p, (64, 64), &k).unwrap();
        assert_ne!(prepares(&mut op) & rgb_enable::SHARPEN, 0);
        let s = op.backend.config().sharpen;
        assert_eq!(s.kernel0, k);
        assert_eq!(s.enables, 1);
    }
    assert_eq!(
        filter5x5(Pixels::Rgb888, (64, 64), &k).unwrap_err(),
        OpError::NoLuma
    );
}

#[test]
fn demosaic_ccm_sets_matrix_and_offsets() {
    let m = [1.5, -0.25, -0.25, -0.5, 1.75, -0.25, 0.0, -0.5, 1.5];
    let mut op = demosaic_ccm(BayerOrder::Bggr, (64, 48), m, [16.0, 0.0, -8.0]).unwrap();
    let en = prepares(&mut op);
    assert_ne!(en & rgb_enable::CCM, 0);
    let c = op.backend.config();
    assert_ne!(c.global.bayer_enables & bayer_enable::DEMOSAIC, 0);
    assert_eq!(c.ccm.coeffs[0], 1536);
    assert_eq!(c.ccm.coeffs[1], -256);
    assert_eq!(c.ccm.offsets, [16 * 257 * 1024, 0, -8 * 257 * 1024]);
    assert_eq!(&op.input_fourcc, b"BYR2");
    assert_eq!(
        demosaic_ccm(BayerOrder::Greyscale, (64, 48), m, [0.0; 3]).unwrap_err(),
        OpError::NotColour
    );
}

#[test]
fn box2_steps_exactly_two_on_phase_0() {
    for p in PIXELS {
        let mut op = box2(p, (1280, 800)).unwrap();
        let tiles = op.backend.prepare().expect("prepare");
        let c = op.backend.config();
        assert_ne!(c.global.rgb_enables & rgb_enable::RESAMPLE0, 0);
        assert_eq!(
            (c.resample[0].scale_factor_h, c.resample[0].scale_factor_v),
            (2 << 12, 2 << 12)
        );
        assert_eq!(c.resample[0].coef, BOX2);
        assert_eq!(op.out.width, 640);
        // Luma's phase in every tile: whole pixels, so tap 2 is the block's first pixel.
        for t in &tiles.tiles[..tiles.num_tiles as usize] {
            assert_eq!(t.resample_phase_x[0] % (1 << 12), 0, "{p:?}");
            assert_eq!(t.resample_phase_y[0] % (1 << 12), 0, "{p:?}");
        }
    }
    // The taps sum to unity and put weight on the pixel and the next one only.
    for phase in BOX2.chunks(6) {
        assert_eq!(phase, [0, 0, 512, 512, 0, 0]);
    }
    assert_eq!(
        box2(Pixels::Grey8, (33, 32)).unwrap_err(),
        OpError::Factor(2)
    );
}

#[test]
fn pyramid2_writes_both_levels() {
    for p in PIXELS {
        let mut op = pyramid2(p, (1280, 800)).unwrap();
        let en = prepares(&mut op);
        assert_eq!(op.output, 0);
        for bit in [
            rgb_enable::RESAMPLE0,
            rgb_enable::OUTPUT0,
            rgb_enable::downscale(1),
            rgb_enable::OUTPUT1,
        ] {
            assert_ne!(en & bit, 0, "{p:?} {bit:#x}");
        }
        assert_eq!(en & rgb_enable::resample(1), 0);
        let c = op.backend.config();
        assert_eq!(c.downscale[1].scale_factor_h, 4 << 12);
        assert_eq!(c.output_format[1].image.width, 320);
        assert_eq!(op.out1.map(|o| (o.width, o.height)), Some((320, 200)));
    }
    assert_eq!(
        pyramid2(Pixels::Grey8, (36, 34)).unwrap_err(),
        OpError::Factor(4)
    );
}

//! Statistics layouts, job encodings, the sRGB default curve, the split of a frame and the
//! refusals of settings the library cannot carry yet.

use styx_aaa::SharpenParams;
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_pisp::pisp::format::formats;
use styx_pisp::pisp::uapi::{BayerOrder, ImageFormatConfig, RawStatistics, Wire};
use styx_pisp::{BackEndBuilder, Geometry, IspSettings, Output, PispError, job, stats};

fn fmt(width: u16, height: u16, format: u32) -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width,
        height,
        format,
        stride: 0,
        stride2: 0,
    };
    styx_pisp::pisp::format::compute_stride_align(&mut f, 64);
    f
}

fn output(code: FourCc, width: u32, height: u32, color: ColorSpace) -> Output {
    Output {
        media: MediaFormat::new(
            code,
            Resolution::new(width, height).expect("non-empty"),
            color,
            ColorRange::Full,
        ),
        stride_align: 64,
    }
}

fn geometry() -> Geometry {
    Geometry {
        input: fmt(1280, 800, formats::BAYER16),
        bayer: BayerOrder::Rggb,
        outputs: [
            output(FourCc::NV12, 1280, 800, ColorSpace::Bt601),
            output(FourCc::RGB3, 640, 400, ColorSpace::Srgb),
        ],
        tdn: None,
        slice_at: None,
    }
}

#[test]
fn a_statistics_buffer_parses_into_the_controllers_form() {
    let mut raw = styx_pisp::pisp::stats::Statistics::default();
    let mut out = styx_aaa::Statistics::default();
    let bytes = vec![0u8; RawStatistics::SIZE];
    stats::parse_into(&mut raw, &bytes, &mut out).expect("a full buffer parses");
    assert_eq!((out.colour.width, out.colour.height), (32, 32));
    assert_eq!(out.colour.zones.len(), 32 * 32);
    assert!(out.before_wb && out.before_lsc);
    let short = vec![0u8; RawStatistics::SIZE - 1];
    assert!(matches!(
        stats::parse_into(&mut raw, &short, &mut out),
        Err(PispError::Stats(_))
    ));
}

#[test]
fn a_job_names_its_buffers_and_encodes() {
    let j = job::back_end_job(7, 3, None, [Some(0), Some(1), None, None]);
    let mut buf = [0u8; job::JOB_CAPACITY];
    let n = job::encode_job(&j, &mut buf).expect("the job encodes");
    assert!(n > 0 && n <= job::JOB_CAPACITY);
    assert_eq!(j.layout, lemnos_contract_isp_pisp::LAYOUT_BACK_END);
    assert_eq!(j.outputs, [0, 1, u32::MAX, u32::MAX]);
    let again = job::encode_job(&j, &mut buf).unwrap();
    assert_eq!(again, n, "the same job encodes the same way");
}

#[test]
fn the_default_tone_curve_is_srgb_on_65_points() {
    let p = styx_pisp::back_end::gamma_points(None);
    assert_eq!(p.len(), 65);
    assert_eq!(p[0], (0, 0));
    assert_eq!(p[64], (65535, 65535));
}

#[test]
fn a_frame_splits_into_two_jobs_for_its_first_rows() {
    let whole = BackEndBuilder::new(&geometry()).expect("the geometry prepares");
    assert!(
        whole.split(600).is_none(),
        "one job per frame without a slice point"
    );
    let mut g = geometry();
    g.slice_at = Some(600);
    let mut b = BackEndBuilder::new(&g).expect("the sliced geometry prepares");
    b.update(&IspSettings::neutral(0.0625), 0.01)
        .expect("a frame");
    let split = b.split(600).expect("a split at the slice point");
    assert!(
        split.rows_after_first.iter().all(|&r| r <= 800),
        "{:?}",
        split.rows_after_first
    );
    let bad = Geometry {
        slice_at: Some(800),
        ..g
    };
    assert!(matches!(
        BackEndBuilder::new(&bad),
        Err(PispError::Config(_))
    ));
}

#[test]
fn settings_the_library_cannot_carry_are_refused_before_a_job() {
    let mut b = BackEndBuilder::new(&geometry()).unwrap();
    // Defective pixel correction has no table in the library at this rev: refused, not written.
    let mut s = IspSettings::neutral(0.0625);
    s.denoise.dpc = 1;
    assert!(matches!(b.update(&s, 0.01), Err(PispError::Unsupported(_))));
    // Sharpening is the library's default now, scaled by the settings' factors.
    let mut s = IspSettings::neutral(0.0625);
    s.sharpen = Some(SharpenParams {
        threshold: 0.5,
        strength: 1.0,
        limit: 1.0,
    });
    b.update(&s, 0.01).expect("sharpening prepares");
    b.update(&IspSettings::neutral(0.0625), 0.01)
        .expect("neutral settings prepare");
}

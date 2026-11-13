//! Stills from the simulated camera: the DNG reads back with the frame's samples and tags, the
//! processed picture is full size, and a standard reader opens the DNG when one is installed.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use styx_aaa::Tuning;
use styx_aaa::tuning::{AwbTuning, CcmTuning, CtCcm};
use styx_dng::color::{Illuminant, ccm_from_color_matrix};
use styx_dng::{CfaPattern, read_dng};
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_settings::IspSettings;
use styx_sim::{Scene, SimCamera, SimFrame};
use styx_still::{Context, StillRequest, serve};

/// The simulated camera's mode 0: 320 by 40.
const MODE: u16 = 0;

/// The software ISP's settings for the still: black level 64 of 1024, a warm white balance.
fn settings() -> IspSettings {
    let mut isp = IspSettings::neutral(64.0 / 1024.0);
    isp.wb = [1.8, 1.0, 1.6];
    isp
}

/// The first frame the camera gives (the stream's first frames can be late).
fn first_frame(camera: &mut SimCamera) -> SimFrame {
    for _ in 0..200 {
        if let Some(frame) = camera.next(Duration::from_millis(100)).expect("frame") {
            return frame;
        }
    }
    panic!("the simulated camera gave no frame");
}

/// The frame's 10-bit samples, decoded here from `pRAA` (independently of the crate under test).
fn praa_samples(frame: &FrameLease<()>, width: u32, height: u32) -> Vec<u16> {
    let read = frame.read().expect("the frame is readable");
    let plane = read.plane(0).expect("one plane");
    let row_bytes = width as usize * 10 / 8;
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height as usize {
        let row = plane.row_bytes(y, row_bytes).expect("a full row");
        for group in row.as_chunks::<5>().0 {
            let low = group[4];
            for (i, &high) in group[..4].iter().enumerate() {
                out.push((u16::from(high) << 2) | u16::from((low >> (2 * i)) & 3));
            }
        }
    }
    out
}

#[test]
fn a_raw_still_from_the_simulated_camera_reads_back_equal() {
    let mut camera = SimCamera::start(Scene::Pattern, MODE).expect("simulated camera");
    let frame = first_frame(&mut camera);
    let (width, height) = (camera.width(), camera.height());
    let isp = settings();
    let tuning = Tuning::default();
    let ctx = Context {
        isp: &isp,
        tuning: &tuning,
        model: "styx-sim",
        unique_camera_model: "Styx styx-sim",
    };

    let still = serve(&StillRequest::raw(), frame.lease(), frame.meta(), &ctx).expect("a still");
    assert_eq!(still.sequence, frame.id());
    assert_eq!(still.timestamp_ns, frame.timestamp_ns());
    assert!(still.picture.is_none());
    let bytes = still.dng.expect("a DNG");
    let file = read_dng(&bytes).expect("the DNG reads back");

    assert_eq!((file.width, file.height), (width, height));
    assert_eq!(file.samples, praa_samples(frame.lease(), width, height));
    assert_eq!(file.cfa, Some(CfaPattern::Rggb));
    assert_eq!(file.white_level, vec![1023]);
    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        assert_eq!(file.black_at(x, y, 0), 64.0, "black level at ({x}, {y})");
    }
    let neutral = file.as_shot_neutral.expect("an as-shot neutral");
    for (got, want) in neutral.iter().zip([1.0 / 1.8, 1.0, 1.0 / 1.6]) {
        assert!((got - want).abs() < 1e-5, "neutral {neutral:?}");
    }
    let exposure = file.exposure_time.expect("an exposure time");
    assert!((exposure - f64::from(frame.meta().exposure_us) / 1e6).abs() < 1e-9);
    assert_eq!(file.iso, Some(100));
    assert_eq!(file.model.as_deref(), Some("styx-sim"));
    assert_eq!(file.unique_camera_model.as_deref(), Some("Styx styx-sim"));
    assert_eq!(file.baseline_exposure, Some(0.0));
    // No AWB curve in the tuning: one calibration, the shot's own CCM and neutral.
    assert_eq!(file.calibrations.len(), 1);
    assert!(
        file.description
            .as_deref()
            .is_some_and(|d| d.contains("exposure_us"))
    );
}

#[test]
fn a_tuned_calibration_reads_back_as_its_ccm() {
    let ccm: [f64; 9] = [
        1.80439, -0.73699, -0.06739, -0.36073, 1.83327, -0.47255, -0.08378, -0.56403, 1.64781,
    ];
    let tuning = Tuning {
        ccm: Some(CcmTuning {
            ccms: vec![CtCcm { ct: 2856.0, ccm }, CtCcm { ct: 6504.0, ccm }],
            saturation: None,
        }),
        awb: Some(AwbTuning {
            ct_curve: vec![[2856.0, 0.9, 0.42], [6504.0, 0.5, 0.75]],
            ..AwbTuning::default()
        }),
        ..Tuning::default()
    };
    let mut camera = SimCamera::start(Scene::Pattern, MODE).expect("simulated camera");
    let frame = first_frame(&mut camera);
    let isp = settings();
    let ctx = Context {
        isp: &isp,
        tuning: &tuning,
        model: "styx-sim",
        unique_camera_model: "",
    };
    let bytes = serve(&StillRequest::raw(), frame.lease(), frame.meta(), &ctx)
        .expect("a still")
        .dng
        .expect("a DNG");
    let file = read_dng(&bytes).expect("reads back");

    assert_eq!(file.calibrations.len(), 2);
    let first = &file.calibrations[0];
    assert_eq!(first.illuminant, Illuminant::StandardA.code());
    let cm = first.color_matrix.expect("a colour matrix");
    let (back, neutral) =
        ccm_from_color_matrix(&cm, Illuminant::StandardA.xy()).expect("invertible");
    for (got, want) in back.iter().zip(ccm) {
        assert!((got - want).abs() < 2e-3, "CCM {back:?}");
    }
    for (got, want) in neutral.iter().zip([0.9, 1.0, 0.42]) {
        assert!((got - want).abs() < 2e-3, "neutral {neutral:?}");
    }
}

#[test]
fn a_processed_still_is_a_full_size_picture() {
    let mut camera = SimCamera::start(Scene::Pattern, MODE).expect("simulated camera");
    let frame = first_frame(&mut camera);
    let (width, height) = (camera.width(), camera.height());
    let isp = settings();
    let tuning = Tuning::default();
    let ctx = Context {
        isp: &isp,
        tuning: &tuning,
        model: "styx-sim",
        unique_camera_model: "",
    };
    let rgb = serve(
        &StillRequest::processed(FourCc::RGB3),
        frame.lease(),
        frame.meta(),
        &ctx,
    )
    .expect("an RGB still")
    .picture
    .expect("a picture");
    assert_eq!(
        (rgb.code, rgb.width, rgb.height),
        (FourCc::RGB3, width, height)
    );
    assert_eq!(rgb.data.len(), width as usize * height as usize * 3);
    assert!(rgb.data.iter().any(|&v| v > 0), "the picture is not black");

    let nv12 = serve(
        &StillRequest::processed(FourCc::NV12),
        frame.lease(),
        frame.meta(),
        &ctx,
    )
    .expect("an NV12 still")
    .picture
    .expect("a picture");
    assert_eq!(nv12.data.len(), width as usize * height as usize * 3 / 2);

    let both = serve(
        &StillRequest::both(FourCc::RGB3),
        frame.lease(),
        frame.meta(),
        &ctx,
    )
    .expect("both");
    assert!(both.dng.is_some() && both.picture.is_some());
    assert_eq!(both.picture.expect("picture").data, rgb.data);
}

/// Runs the standard readers that are installed on the DNG: `exiv2` must list its tags, and
/// ImageMagick (through LibRaw) must decode it at the frame's size. Prints which ran; skips with
/// a reason when neither is installed.
#[test]
fn a_still_opens_in_a_standard_reader() {
    let mut camera = SimCamera::start(Scene::Pattern, MODE).expect("simulated camera");
    let frame = first_frame(&mut camera);
    let (width, height) = (camera.width(), camera.height());
    let isp = settings();
    let tuning = Tuning::default();
    let ctx = Context {
        isp: &isp,
        tuning: &tuning,
        model: "styx-sim",
        unique_camera_model: "",
    };
    let bytes = serve(&StillRequest::raw(), frame.lease(), frame.meta(), &ctx)
        .expect("a still")
        .dng
        .expect("a DNG");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("sim-still.dng");
    std::fs::write(&path, &bytes).expect("write the DNG");

    let have = |tool: &str, flag: &str| {
        Command::new(tool)
            .arg(flag)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let exiv2 = have("exiv2", "--version");
    let magick = have("magick", "-version");
    if !exiv2 && !magick {
        eprintln!("SKIPPED: no standard DNG reader installed (exiv2, magick)");
        return;
    }
    let mut ran = Vec::new();
    if exiv2 {
        let out = Command::new("exiv2")
            .arg("-pa")
            .arg(&path)
            .output()
            .expect("exiv2 runs");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "exiv2: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("DNGVersion"),
            "exiv2 lists the DNG tags:\n{text}"
        );
        ran.push("exiv2 (tags)");
    }
    if magick {
        let png = dir.join("sim-still.png");
        let out = Command::new("magick")
            .arg(&path)
            .arg(&png)
            .output()
            .expect("magick runs");
        assert!(
            out.status.success(),
            "magick: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let size = Command::new("magick")
            .arg("identify")
            .args(["-format", "%w %h"])
            .arg(&png)
            .output()
            .expect("magick identify runs");
        assert_eq!(
            String::from_utf8_lossy(&size.stdout).trim(),
            format!("{width} {height}")
        );
        ran.push("magick (LibRaw decode)");
    }
    eprintln!("standard readers that ran: {}", ran.join(", "));
}

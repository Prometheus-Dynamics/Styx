use std::time::{Duration, Instant};

use super::*;
use crate::camera::{CameraOptions, NativeCamera, StreamSettings};
use crate::stream::FrameStream;

/// The built-in descriptions only (nothing from the system's paths).
fn library() -> SensorLibrary {
    styx_sensor::BUILTIN_DESCRIPTIONS
        .iter()
        .fold(SensorLibrary::new(), |l, (name, toml)| {
            l.with_embedded(*name, *toml)
        })
}

/// Opens a simulated camera of `spec` (with a key of its own, so tests run in parallel).
fn open(mut spec: SimSpec, index: u32) -> NativeCamera {
    spec.index = index;
    let info = camera_info(&spec, &library()).unwrap();
    rig::open_spec(info, CameraOptions::default(), Instant::now(), spec).unwrap()
}

/// The mean 10-bit sample of a CSI-2 packed RAW10 frame's high bytes (approximate).
fn mean(frame: &crate::buffers::NativeFrame) -> f64 {
    let data = frame.data();
    let (stride, w) = (frame.stride as usize, frame.width as usize);
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in (0..frame.height as usize).step_by(8) {
        let line = &data[y * stride..y * stride + w / 4 * 5];
        for group in line.as_chunks::<5>().0 {
            for b in &group[..4] {
                sum += u64::from(*b) << 2;
                n += 1;
            }
        }
    }
    sum as f64 / n.max(1) as f64
}

fn next(stream: &mut FrameStream) -> crate::buffers::NativeFrame {
    stream
        .next_blocking(Duration::from_secs(2))
        .unwrap()
        .expect("a frame")
}

/// The camera opens, configures and streams as a real one: the description's modes, CSI-2
/// packed RAW10 at the rate asked for, timestamps from the monotonic clock one frame apart,
/// each frame carrying the controls that made it.
#[test]
fn a_simulated_camera_streams_like_the_sensor() {
    let mut spec = SimSpec::new("ov9782");
    spec.source = SimSource::Flat {
        grey: 0.18,
        light: 1.0,
    };
    spec.noise = false;
    let mut cam = open(spec, 10);
    assert!(cam.info().display_name().contains("simulated"));
    assert!(
        cam.info()
            .modes
            .iter()
            .any(|m| (m.width, m.height) == (1280, 800))
    );
    // 640x400 at 30 fps: frames render in time in an unoptimised test build too.
    let c = cam
        .configure(&StreamSettings::new(640, 400).fps(30))
        .unwrap();
    assert_eq!(c.fourcc, FourCc::new(b"pBAA"));
    let mut stream = cam.start().unwrap();
    // Frames go back as they are read (the queue has 4 buffers).
    let stamps: Vec<Duration> = (0..7).map(|_| next(&mut stream).timestamp).collect();
    let span = stamps[6] - stamps[0];
    let fps = 6.0 / span.as_secs_f64();
    assert!((fps - 30.0).abs() < 3.0, "{fps:.1} fps");
    let f = &next(&mut stream);
    assert!(!f.error);
    assert_eq!((f.width, f.height), (640, 400));
    let applied = f.controls.expect("the frame's controls");
    // The flat grey at the frame's exposure and gain, over the black level (64 of 1023).
    let expected = 64.0 + 0.18 * 100.0 * applied.exposure.as_secs_f64() * applied.gain() * 959.0;
    let got = mean(f);
    assert!(
        (got - expected).abs() < expected * 0.05 + 3.0,
        "{got} vs {expected}"
    );
    drop(stream);
    cam.stop().unwrap();
}

/// Exposure set through the camera's controls lands on the frame the control schedule says,
/// and the frames brighten accordingly; flips rename the frames' format.
#[test]
fn exposure_lands_on_its_frame_and_flips_rename_the_format() {
    let mut spec = SimSpec::new("ov9782");
    spec.source = SimSource::Flat {
        grey: 0.18,
        light: 1.0,
    };
    spec.noise = false;
    let mut cam = open(spec, 11);
    cam.configure(&StreamSettings::new(1280, 800).fps(30))
        .unwrap();
    let controls = cam.controls();
    let mut stream = cam.start().unwrap();
    let first = next(&mut stream);
    let before = mean(&first);
    let landings = controls.set_exposure(Duration::from_millis(20)).unwrap();
    let landing = landings.iter().map(|l| l.frame).max().unwrap();
    let mut seen = None;
    for _ in 0..12 {
        let f = next(&mut stream);
        if mean(&f) > before * 1.3 {
            seen = Some(u64::from(f.sequence));
            break;
        }
    }
    assert_eq!(
        seen,
        Some(landing),
        "brighter first at {seen:?}, predicted {landing}"
    );
    drop(stream);
    cam.stop().unwrap();
    let c = cam
        .configure(&StreamSettings::new(1280, 800).fps(30).flips(true, true))
        .unwrap();
    assert_eq!(c.fourcc, FourCc::new(b"pRAA"));
    let mut stream = cam.start().unwrap();
    assert_eq!(next(&mut stream).fourcc, FourCc::new(b"pRAA"));
}

#[test]
fn specs_parse() {
    let specs = parse_specs(
        "ov9782,scene=flat,grey=0.5,drop=0.01,jitter_ms=2 # a comment\n\
         ov9782,recording=/tmp/a.mcap,unplug_every_s=30;imx219",
    )
    .unwrap();
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[0].key(), "sim:ov9782:0");
    assert_eq!(specs[1].key(), "sim:ov9782:1");
    assert_eq!(specs[2].key(), "sim:imx219:0");
    assert_eq!(
        specs[0].source,
        SimSource::Flat {
            grey: 0.5,
            light: 1.0
        }
    );
    assert_eq!(specs[0].faults.jitter, Duration::from_millis(2));
    assert_eq!(specs[1].faults.unplug_for, Duration::from_secs(2));
    assert!(SimSpec::parse("ov9782,bogus=1").is_err());
    assert!(SimSpec::parse("scene=chart").is_err());
}

/// Faults: lost frames leave gaps in the sequence; a sensor that disappears ends the stream,
/// is not listed or opened while away, and comes back.
#[test]
fn lost_frames_and_a_sensor_that_goes_away() {
    let mut spec = SimSpec::new("ov9782");
    spec.faults.drop = 0.3;
    spec.faults.unplug_every = Some(Duration::from_millis(600));
    spec.faults.unplug_for = Duration::from_millis(400);
    spec.faults.seed = 7;
    let mut cam = open(spec.clone(), 12);
    cam.configure(&StreamSettings::new(640, 400).fps(60))
        .unwrap();
    let mut stream = cam.start().unwrap();
    let mut last = None;
    let mut gaps = 0;
    let ended = loop {
        match stream.next_blocking(Duration::from_secs(2)) {
            Ok(Some(f)) => {
                if let Some(l) = last
                    && f.sequence > l + 1
                {
                    gaps += 1;
                }
                last = Some(f.sequence);
            }
            Ok(None) | Err(_) => break true,
        }
    };
    assert!(ended && gaps > 0, "gaps {gaps}");
    let key = {
        let mut s = spec.clone();
        s.index = 12;
        s.key()
    };
    assert!(!present(&key));
    drop(stream);
    drop(cam);
    std::thread::sleep(Duration::from_millis(500));
    assert!(present(&key));
    let _again = open(spec, 12);
}

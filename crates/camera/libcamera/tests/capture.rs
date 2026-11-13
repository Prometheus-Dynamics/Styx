//! Runs against a real libcamera camera when one is present; otherwise each test prints why it
//! skipped and passes. Build with `--features libcamera` on a machine with libcamera.
//!
//! The zero-copy assertion below is not yet run on a camera: it passes only when a camera is
//! present, and that proof runs on a board in PR H6.

use std::sync::mpsc;
use std::time::Duration;

use styx_format::FourCc;
use styx_libcamera::{CaptureConfig, Error};

fn skip(reason: &str) {
    eprintln!("SKIP styx-libcamera test: {reason}");
}

#[test]
fn lists_cameras_or_skips() {
    match styx_libcamera::list_cameras() {
        Err(Error::Unavailable(reason)) => skip(reason),
        Err(err) => panic!("listing cameras failed: {err}"),
        Ok(cameras) if cameras.is_empty() => skip("libcamera lists no cameras"),
        Ok(cameras) => {
            for camera in &cameras {
                eprintln!(
                    "camera {} offers {} formats",
                    camera.id,
                    camera.formats.len()
                );
            }
        }
    }
}

#[test]
fn captures_frames_without_copies() {
    let cameras = match styx_libcamera::list_cameras() {
        Err(Error::Unavailable(reason)) => return skip(reason),
        Err(err) => panic!("listing cameras failed: {err}"),
        Ok(cameras) => cameras,
    };
    let Some(camera) = cameras.iter().find(|c| !c.formats.is_empty()) else {
        return skip("no camera with a format");
    };
    let format = camera.formats[0];
    let config = CaptureConfig {
        code: format.code,
        width: format.width,
        height: format.height,
        buffers: 4,
    };
    let (tx, rx) = mpsc::channel();
    let capture = styx_libcamera::Capture::open(
        &camera.id,
        config,
        Box::new(move |lease| {
            let _ = tx.send(lease.id());
        }),
    )
    .expect("open camera");
    let mut seen = 0;
    while seen < 30 {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(_) => seen += 1,
            Err(_) => break,
        }
    }
    let stats = capture.stats();
    eprintln!(
        "camera {} {:?}: {seen} frames, stats {stats:?}",
        camera.id,
        FourCc::NV12
    );
    assert!(seen > 0, "no frames from {}", camera.id);
    for counter in styx_frame::copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
}

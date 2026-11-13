//! The backend with FFmpeg's libraries not present: the candidates are skipped with a reason.
#![cfg(all(feature = "ffmpeg", target_os = "linux"))]

use std::path::PathBuf;

use styx_core::{CodecKind, Registry, Request, Verdict};
use styx_format::FourCc;

#[test]
fn absent_libraries_skip_the_candidates_with_a_reason() {
    let dirs = [PathBuf::from("/nonexistent/styx-ffmpeg-absent-test")];
    let why = styx_ffmpeg::available_in(&dirs).expect_err("the libraries are not there");
    assert!(why.starts_with("FFmpeg not present"), "{why}");
    eprintln!("absent as expected: {why}");

    let mut registry: Registry = Registry::new();
    styx_ffmpeg::register_in(&mut registry, &dirs);
    let error = registry
        .select(Request::new(CodecKind::Encoder, FourCc::I420, FourCc::MJPG))
        .expect_err("no candidate can build without FFmpeg");
    let skipped: Vec<&str> = error
        .decisions
        .iter()
        .filter_map(|d| match &d.verdict {
            Verdict::Skipped(why) => Some(why.as_str()),
            _ => None,
        })
        .collect();
    let missing: Vec<&&str> = skipped
        .iter()
        .filter(|w| w.contains("FFmpeg not present"))
        .collect();
    assert_eq!(missing.len(), 1, "{error}");
    eprintln!("{error}");
}

#[test]
fn default_registry_without_the_feature_offers_no_ffmpeg_candidate() {
    // With the feature on, `register` is the only way in; an empty registry stays empty.
    let registry: Registry = Registry::new();
    assert!(registry.candidates().is_empty());
}

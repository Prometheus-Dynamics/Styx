//! The loaded libraries report the majors Styx has laid out.
#![cfg(all(feature = "ffmpeg", target_os = "linux"))]

#[test]
fn loaded_library_versions_are_supported() {
    let (util, codec) = match styx_ffmpeg::loaded_versions() {
        Ok(versions) => versions,
        Err(why) => {
            eprintln!("skipped loaded_library_versions_are_supported: {why}");
            return;
        }
    };
    eprintln!("loaded libavutil major {util}, libavcodec major {codec}");
    assert!(
        styx_ffmpeg::SUPPORTED.contains(&(util, codec)),
        "libavutil {util} / libavcodec {codec} is not in {:?}",
        styx_ffmpeg::SUPPORTED
    );
}

#[test]
fn supported_list_is_the_laid_out_majors() {
    // The layouts in `sys` were written for FFmpeg 8.x; a new major is added here only after its
    // field offsets are checked.
    assert_eq!(styx_ffmpeg::SUPPORTED, &[(60, 62)]);
}

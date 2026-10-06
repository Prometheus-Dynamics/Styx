//! With `codec-ffmpeg`: sets `ffmpeg_codec_pix_fmts` when the FFmpeg headers ffmpeg-sys-next
//! found still have `AVCodec.pix_fmts` (removed in FFmpeg 9.0), read for encoder pixel formats on
//! FFmpeg before 7.1. Other builds do nothing here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(ffmpeg_codec_pix_fmts)");
    // ffmpeg-sys-next (`links = "ffmpeg"`) exports `ffmpeg_9_0=true` for FFmpeg 9.0+ headers.
    println!("cargo:rerun-if-env-changed=DEP_FFMPEG_FFMPEG_9_0");
    if std::env::var_os("CARGO_FEATURE_CODEC_FFMPEG").is_some()
        && std::env::var("DEP_FFMPEG_FFMPEG_9_0").as_deref() != Ok("true")
    {
        println!("cargo:rustc-cfg=ffmpeg_codec_pix_fmts");
    }
}

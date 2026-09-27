//! Raw FFmpeg types and the few raw functions Styx calls directly.
//!
//! Types come from `ffmpeg-sys-next`. Functions are forwarders to the runtime-loaded libraries
//! (see the `loader` module); they are listed explicitly so no call can link FFmpeg by accident.
//! Callers only reach these after an FFmpeg codec was created, so the libraries are loaded.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int};

pub use ffmpeg_sys_next::{
    AVBufferRef, AVCodec, AVCodecContext, AVCodecHWConfig, AVCodecID, AVCodecParameters,
    AVDRMFrameDescriptor, AVDictionary, AVFrame, AVHWDeviceType, AVHWFramesContext, AVMediaType,
    AVPacket, AVPixelFormat, AVRational,
};
pub use std::ffi::c_void;

use super::loader::loaded;

/// `AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX` (libavcodec/codec.h).
pub const AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX: c_int = 0x01;
/// `AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX` (libavcodec/codec.h).
pub const AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX: c_int = 0x02;
/// `AV_HWFRAME_MAP_READ` (libavutil/hwcontext.h).
pub const AV_HWFRAME_MAP_READ: c_int = 1;
/// `AV_HWFRAME_MAP_DIRECT` (libavutil/hwcontext.h): map without copying, or fail.
pub const AV_HWFRAME_MAP_DIRECT: c_int = 8;
/// `AV_CODEC_HW_CONFIG_METHOD_INTERNAL` (libavcodec/codec.h).
pub const AV_CODEC_HW_CONFIG_METHOD_INTERNAL: c_int = 0x04;

pub unsafe fn av_buffer_unref(buf: *mut *mut AVBufferRef) {
    unsafe { (loaded().util.av_buffer_unref)(buf) }
}

pub unsafe fn av_buffer_ref(buf: *const AVBufferRef) -> *mut AVBufferRef {
    unsafe { (loaded().util.av_buffer_ref)(buf) }
}

pub unsafe fn av_hwdevice_ctx_create(
    device_ctx: *mut *mut AVBufferRef,
    kind: AVHWDeviceType,
    device: *const c_char,
    opts: *mut AVDictionary,
    flags: c_int,
) -> c_int {
    unsafe { (loaded().util.av_hwdevice_ctx_create)(device_ctx, kind, device, opts, flags) }
}

pub unsafe fn av_hwframe_transfer_data(
    dst: *mut AVFrame,
    src: *const AVFrame,
    flags: c_int,
) -> c_int {
    unsafe { (loaded().util.av_hwframe_transfer_data)(dst, src, flags) }
}

pub unsafe fn av_frame_copy_props(dst: *mut AVFrame, src: *const AVFrame) -> c_int {
    unsafe { (loaded().util.av_frame_copy_props)(dst, src) }
}

pub unsafe fn avcodec_get_hw_config(codec: *const AVCodec, index: c_int) -> *const AVCodecHWConfig {
    unsafe { (loaded().codec.avcodec_get_hw_config)(codec, index) }
}

pub unsafe fn av_hwframe_ctx_alloc(device: *mut AVBufferRef) -> *mut AVBufferRef {
    unsafe { (loaded().util.av_hwframe_ctx_alloc)(device) }
}

pub unsafe fn av_hwframe_ctx_init(frames: *mut AVBufferRef) -> c_int {
    unsafe { (loaded().util.av_hwframe_ctx_init)(frames) }
}

pub unsafe fn av_hwframe_get_buffer(
    frames: *mut AVBufferRef,
    frame: *mut AVFrame,
    flags: c_int,
) -> c_int {
    unsafe { (loaded().util.av_hwframe_get_buffer)(frames, frame, flags) }
}

pub unsafe fn av_hwframe_map(dst: *mut AVFrame, src: *const AVFrame, flags: c_int) -> c_int {
    unsafe { (loaded().util.av_hwframe_map)(dst, src, flags) }
}

pub unsafe fn av_hwframe_ctx_create_derived(
    derived: *mut *mut AVBufferRef,
    format: AVPixelFormat,
    device: *mut AVBufferRef,
    source: *mut AVBufferRef,
    flags: c_int,
) -> c_int {
    unsafe { (loaded().util.av_hwframe_ctx_create_derived)(derived, format, device, source, flags) }
}

pub unsafe fn av_hwdevice_ctx_create_derived(
    derived: *mut *mut AVBufferRef,
    kind: AVHWDeviceType,
    source: *mut AVBufferRef,
    flags: c_int,
) -> c_int {
    unsafe { (loaded().util.av_hwdevice_ctx_create_derived)(derived, kind, source, flags) }
}

pub unsafe fn av_buffer_create(
    data: *mut u8,
    size: usize,
    free: Option<unsafe extern "C" fn(*mut c_void, *mut u8)>,
    opaque: *mut c_void,
    flags: c_int,
) -> *mut AVBufferRef {
    unsafe { (loaded().util.av_buffer_create)(data, size, free, opaque, flags) }
}

//! The FFmpeg struct fields Styx reads and writes, by offset, and no others.
//!
//! No libavcodec context field is touched: codec parameters go through the AVOptions API
//! (`video_size`, `pixel_format`, `time_base`, set with `av_opt_set`), and decoded geometry is read
//! from the frame. Only the leading public fields of `AVFrame` and `AVPacket` are used, and only
//! those are laid out here. Each layout holds for the majors in [`crate::SUPPORTED`]:
//!
//! - `AVFrame` (libavutil 60, FFmpeg 8.x): `data[8]` at 0 (8-byte pointers), `linesize[8]` at 64
//!   (`int`s), `extended_data` at 96, `width` at 104, `height` at 108, `nb_samples` at 112,
//!   `format` at 116. Frames are allocated only with `av_frame_alloc`.
//! - `AVPacket` (libavcodec 62, FFmpeg 8.x): `buf` at 0, `pts` at 8 (`int64`), `dts` at 16,
//!   `data` at 24, `size` at 32 (`int`). Packets are allocated only with `av_packet_alloc`.
//!
//! The frame's own `pts` is not used: its offset is outside this list. A frame's timestamp is the
//! lease it came from, which is exact for the one-packet-in, one-frame-out calls this backend
//! makes. A codec that reorders frames would need the frame's pts (a later PR).
//!
//! A DRM PRIME frame's `data[0]` is an `AVDRMFrameDescriptor` ([`AvDrmFrame`]), laid out by hand
//! below for libavutil 60 with the same care.
//!
//! The tests check these: `av_frame_get_buffer` fills `linesize` and reads `width`, `height` and
//! `format`, and a decode of an encoded frame returns the geometry it was given.

use std::ffi::{c_int, c_void};

pub(crate) const FRAME_DATA: usize = 0;
pub(crate) const FRAME_LINESIZE: usize = 64;
pub(crate) const FRAME_WIDTH: usize = 104;
pub(crate) const FRAME_HEIGHT: usize = 108;
pub(crate) const FRAME_FORMAT: usize = 116;

pub(crate) const PACKET_PTS: usize = 8;
pub(crate) const PACKET_DATA: usize = 24;
pub(crate) const PACKET_SIZE: usize = 32;

/// `AVPixelFormat` values Styx uses (libavutil/pixfmt.h).
pub(crate) const PIX_YUV420P: c_int = 0;
pub(crate) const PIX_YUYV422: c_int = 1;
pub(crate) const PIX_GRAY8: c_int = 8;
pub(crate) const PIX_YUVJ420P: c_int = 12;
pub(crate) const PIX_NV12: c_int = 23;

/// `AVERROR(EAGAIN)` on Linux: the codec needs more input, or has no output yet.
pub(crate) const ERR_EAGAIN: c_int = -11;

/// Reads the `c_int` at `offset` bytes into the struct at `base`.
///
/// # Safety
///
/// `base` must point to a live FFmpeg struct with a readable `c_int` at `offset`.
pub(crate) unsafe fn read_int(base: *const c_void, offset: usize) -> c_int {
    // SAFETY: the caller guarantees a readable, aligned-enough c_int at this offset; unaligned read
    // is used to be safe about the field's alignment.
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<c_int>()
            .read_unaligned()
    }
}

/// Writes a `c_int` at `offset` bytes into the struct at `base`.
///
/// # Safety
///
/// `base` must point to a live FFmpeg struct with a writable `c_int` at `offset`.
pub(crate) unsafe fn write_int(base: *mut c_void, offset: usize, value: c_int) {
    // SAFETY: the caller guarantees a writable c_int at this offset.
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<c_int>()
            .write_unaligned(value)
    }
}

/// Writes the `i64` at `offset`.
///
/// # Safety
///
/// As [`write_int`], for an `i64`.
pub(crate) unsafe fn write_i64(base: *mut c_void, offset: usize, value: i64) {
    // SAFETY: as in `write_int`.
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<i64>()
            .write_unaligned(value)
    }
}

/// Reads the pointer at `offset`.
///
/// # Safety
///
/// As [`read_int`], for a pointer.
pub(crate) unsafe fn read_ptr(base: *const c_void, offset: usize) -> *mut u8 {
    // SAFETY: as in `read_int`.
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<*mut u8>()
            .read_unaligned()
    }
}

/// `AV_DRM_MAX_PLANES` (libavutil/hwcontext_drm.h): the most objects, layers and planes a DRM
/// PRIME descriptor holds.
pub(crate) const DRM_MAX: usize = 4;

/// `AVDRMObjectDescriptor`, one dma-buf: `int fd` at 0, `size_t size` at 8, `uint64_t
/// format_modifier` at 16. 24 bytes on LP64.
#[repr(C)]
pub(crate) struct AvDrmObject {
    pub(crate) fd: c_int,
    pub(crate) size: usize,
    pub(crate) format_modifier: u64,
}

/// `AVDRMPlaneDescriptor`: `int object_index` at 0, `ptrdiff_t offset` at 8, `ptrdiff_t pitch` at
/// 16. 24 bytes on LP64.
#[repr(C)]
pub(crate) struct AvDrmPlane {
    pub(crate) object_index: c_int,
    pub(crate) offset: isize,
    pub(crate) pitch: isize,
}

/// `AVDRMLayerDescriptor`: `uint32_t format` at 0, `int nb_planes` at 4, `planes[4]` at 8. 104
/// bytes on LP64.
#[repr(C)]
pub(crate) struct AvDrmLayer {
    pub(crate) format: u32,
    pub(crate) nb_planes: c_int,
    pub(crate) planes: [AvDrmPlane; DRM_MAX],
}

/// `AVDRMFrameDescriptor`, the `data[0]` of an `AV_PIX_FMT_DRM_PRIME` frame: `int nb_objects` at
/// 0, `objects[4]` at 8, `int nb_layers` at 104, `layers[4]` at 112. 528 bytes on LP64.
///
/// Laid out from libavutil 60's hwcontext_drm.h (FFmpeg 8.x), the header the public struct is
/// declared in. The offsets are checked below and in the tests.
#[repr(C)]
pub(crate) struct AvDrmFrame {
    pub(crate) nb_objects: c_int,
    pub(crate) objects: [AvDrmObject; DRM_MAX],
    pub(crate) nb_layers: c_int,
    pub(crate) layers: [AvDrmLayer; DRM_MAX],
}

const _: () = {
    // The descriptor's size_t and ptrdiff_t fields are 8 bytes: the layout above is LP64.
    assert!(core::mem::size_of::<usize>() == 8);
    assert!(core::mem::size_of::<AvDrmObject>() == 24);
    assert!(core::mem::size_of::<AvDrmPlane>() == 24);
    assert!(core::mem::size_of::<AvDrmLayer>() == 104);
    assert!(core::mem::offset_of!(AvDrmFrame, objects) == 8);
    assert!(core::mem::offset_of!(AvDrmFrame, nb_layers) == 104);
    assert!(core::mem::offset_of!(AvDrmFrame, layers) == 112);
    assert!(core::mem::size_of::<AvDrmFrame>() == 528);
};

/// Writes the pointer at `offset`.
///
/// # Safety
///
/// As [`write_int`], for a pointer.
#[cfg(test)]
pub(crate) unsafe fn write_ptr(base: *mut c_void, offset: usize, value: *mut c_void) {
    // SAFETY: the caller guarantees a writable pointer-sized slot at this offset.
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<*mut c_void>()
            .write_unaligned(value)
    }
}

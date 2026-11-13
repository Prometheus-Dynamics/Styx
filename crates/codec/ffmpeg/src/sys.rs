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

//! The FFmpeg structs Styx touches, by field offset.
//!
//! The FFmpeg headers are not installed where this is written, so the layouts are not generated.
//! Styx reads and writes only the leading fields below, whose offsets have been the same across
//! FFmpeg 6 to 8 (`AVFrame`: `data` at 0, `linesize` at 64, `width` and `height` at 104 and 108,
//! `format` at 116, `pts` at 136; `AVPacket`: `pts` at 8, `data` at 24, `size` at 32). The tests
//! check them against the library: `av_frame_get_buffer` and `av_new_packet` fill those fields,
//! and a round trip through the codec must return the timestamps that went in.

use std::ffi::{c_int, c_void};

pub(crate) const FRAME_DATA: usize = 0;
pub(crate) const FRAME_LINESIZE: usize = 64;
pub(crate) const FRAME_WIDTH: usize = 104;
pub(crate) const FRAME_HEIGHT: usize = 108;
pub(crate) const FRAME_FORMAT: usize = 116;
pub(crate) const FRAME_PTS: usize = 136;

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

/// Reads the `i64` at `offset`.
///
/// # Safety
///
/// As [`read_int`], for an `i64`.
pub(crate) unsafe fn read_i64(base: *const c_void, offset: usize) -> i64 {
    // SAFETY: as in `read_int`.
    unsafe { base.cast::<u8>().add(offset).cast::<i64>().read_unaligned() }
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

//! The V4L2 uAPI structures and ioctl numbers (linux/videodev2.h), written by hand.
//!
//! Only the fields and union members this crate uses are named; unions are represented with
//! their full size and alignment so the layouts match the kernel's exactly. Layouts are checked
//! at compile time on 64-bit targets.

#![allow(non_camel_case_types)]

use std::mem::offset_of;

use crate::geometry::{Fraction, Rect};
use crate::ioctl::{ioctls, ior, iow, iowr};

pub const VIDEO_MAX_PLANES: usize = 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_fmtdesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`; the discrete/stepwise union is `u` (discrete uses `u[0..2]`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_frmsizeenum {
    pub index: u32,
    pub pixel_format: u32,
    pub type_: u32,
    pub u: [u32; 6],
    pub reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`; the union holds one fraction (discrete) or three (stepwise).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_frmivalenum {
    pub index: u32,
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub type_: u32,
    pub u: [Fraction; 3],
    pub reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_pix_format {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_plane_pix_format {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_pix_format_mplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [v4l2_plane_pix_format; VIDEO_MAX_PLANES],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_meta_format {
    pub dataformat: u32,
    pub buffersize: u32,
    pub width: u32,
    pub height: u32,
    pub bytesperline: u32,
}

/// The `fmt` union of `struct v4l2_format`. `v4l2_window` (not used here) contains pointers,
/// which gives the union pointer alignment; `_align` reproduces that.
#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_format_union {
    pub pix: v4l2_pix_format,
    pub pix_mp: v4l2_pix_format_mplane,
    pub meta: v4l2_meta_format,
    pub raw_data: [u8; 200],
    pub _align: [*mut libc::c_void; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_format {
    pub type_: u32,
    pub fmt: v4l2_format_union,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_captureparm {
    pub capability: u32,
    pub capturemode: u32,
    pub timeperframe: Fraction,
    pub extendedmode: u32,
    pub readbuffers: u32,
    pub reserved: [u32; 4],
}

/// `struct v4l2_streamparm`; `v4l2_outputparm` has the same layout as `v4l2_captureparm`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_streamparm_union {
    pub capture: v4l2_captureparm,
    pub raw_data: [u8; 200],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_streamparm {
    pub type_: u32,
    pub parm: v4l2_streamparm_union,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_requestbuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

/// The `m` union of `v4l2_plane` / `v4l2_buffer`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_m {
    pub offset: u32,
    pub userptr: libc::c_ulong,
    pub planes: *mut v4l2_plane,
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: v4l2_m,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: v4l2_timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: v4l2_m,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_exportbuffer {
    pub type_: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_queryctrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
    pub flags: u32,
    pub reserved: [u32; 2],
}

pub const V4L2_CTRL_MAX_DIMS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_query_ext_ctrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i64,
    pub maximum: i64,
    pub step: u64,
    pub default_value: i64,
    pub flags: u32,
    pub elem_size: u32,
    pub elems: u32,
    pub nr_of_dims: u32,
    pub dims: [u32; V4L2_CTRL_MAX_DIMS],
    pub reserved: [u32; 32],
}

/// `struct v4l2_querymenu`: `name` overlays the `__s64 value` of integer menus.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct v4l2_querymenu {
    pub id: u32,
    pub index: u32,
    pub name: [u8; 32],
    pub reserved: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub union v4l2_ext_control_value {
    pub value: i32,
    pub value64: i64,
    pub ptr: *mut libc::c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct v4l2_ext_control {
    pub id: u32,
    pub size: u32,
    pub reserved2: [u32; 1],
    pub u: v4l2_ext_control_value,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_ext_controls {
    pub which: u32,
    pub count: u32,
    pub error_idx: u32,
    pub request_fd: i32,
    pub reserved: [u32; 1],
    pub controls: *mut v4l2_ext_control,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_selection {
    pub type_: u32,
    pub target: u32,
    pub flags: u32,
    pub r: Rect,
    pub reserved: [u32; 9],
}

/// The 64-byte event payload union of `struct v4l2_event` (8-byte aligned: `v4l2_event_ctrl`
/// has an `__s64`).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct v4l2_event_data(pub [u8; 64]);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_event {
    pub type_: u32,
    pub u: v4l2_event_data,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: libc::timespec,
    pub id: u32,
    pub reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_event_subscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

ioctls! {
    VIDIOC_QUERYCAP = ior::<v4l2_capability>(b'V', 0);
    VIDIOC_ENUM_FMT = iowr::<v4l2_fmtdesc>(b'V', 2);
    VIDIOC_G_FMT = iowr::<v4l2_format>(b'V', 4);
    VIDIOC_S_FMT = iowr::<v4l2_format>(b'V', 5);
    VIDIOC_REQBUFS = iowr::<v4l2_requestbuffers>(b'V', 8);
    VIDIOC_QUERYBUF = iowr::<v4l2_buffer>(b'V', 9);
    VIDIOC_QBUF = iowr::<v4l2_buffer>(b'V', 15);
    VIDIOC_EXPBUF = iowr::<v4l2_exportbuffer>(b'V', 16);
    VIDIOC_DQBUF = iowr::<v4l2_buffer>(b'V', 17);
    VIDIOC_STREAMON = iow::<libc::c_int>(b'V', 18);
    VIDIOC_STREAMOFF = iow::<libc::c_int>(b'V', 19);
    VIDIOC_G_PARM = iowr::<v4l2_streamparm>(b'V', 21);
    VIDIOC_S_PARM = iowr::<v4l2_streamparm>(b'V', 22);
    VIDIOC_QUERYCTRL = iowr::<v4l2_queryctrl>(b'V', 36);
    VIDIOC_QUERYMENU = iowr::<v4l2_querymenu>(b'V', 37);
    VIDIOC_TRY_FMT = iowr::<v4l2_format>(b'V', 64);
    VIDIOC_G_EXT_CTRLS = iowr::<v4l2_ext_controls>(b'V', 71);
    VIDIOC_S_EXT_CTRLS = iowr::<v4l2_ext_controls>(b'V', 72);
    VIDIOC_TRY_EXT_CTRLS = iowr::<v4l2_ext_controls>(b'V', 73);
    VIDIOC_ENUM_FRAMESIZES = iowr::<v4l2_frmsizeenum>(b'V', 74);
    VIDIOC_ENUM_FRAMEINTERVALS = iowr::<v4l2_frmivalenum>(b'V', 75);
    VIDIOC_DQEVENT = ior::<v4l2_event>(b'V', 89);
    VIDIOC_SUBSCRIBE_EVENT = iow::<v4l2_event_subscription>(b'V', 90);
    VIDIOC_UNSUBSCRIBE_EVENT = iow::<v4l2_event_subscription>(b'V', 91);
    VIDIOC_G_SELECTION = iowr::<v4l2_selection>(b'V', 94);
    VIDIOC_S_SELECTION = iowr::<v4l2_selection>(b'V', 95);
    VIDIOC_QUERY_EXT_CTRL = iowr::<v4l2_query_ext_ctrl>(b'V', 103);
}

/// Returns a zero-initialised uAPI struct.
pub fn zeroed<T: Copy>() -> T {
    // SAFETY: only used for the plain-data uAPI structs in this crate, for which all-zero bytes
    // (integers, null pointers, empty unions) are a valid value.
    unsafe { std::mem::zeroed() }
}

// Layouts common to all targets.
const _: () = {
    assert!(size_of::<v4l2_capability>() == 104);
    assert!(size_of::<v4l2_fmtdesc>() == 64);
    assert!(size_of::<v4l2_frmsizeenum>() == 44);
    assert!(size_of::<v4l2_frmivalenum>() == 52);
    assert!(size_of::<v4l2_pix_format>() == 48);
    assert!(size_of::<v4l2_plane_pix_format>() == 20);
    assert!(size_of::<v4l2_pix_format_mplane>() == 192);
    assert!(offset_of!(v4l2_pix_format_mplane, num_planes) == 180);
    assert!(size_of::<v4l2_meta_format>() == 20);
    assert!(size_of::<v4l2_captureparm>() == 40);
    assert!(size_of::<v4l2_streamparm>() == 204);
    assert!(size_of::<v4l2_requestbuffers>() == 20);
    assert!(size_of::<v4l2_timecode>() == 16);
    assert!(size_of::<v4l2_exportbuffer>() == 64);
    assert!(size_of::<v4l2_queryctrl>() == 68);
    assert!(size_of::<v4l2_query_ext_ctrl>() == 232);
    assert!(offset_of!(v4l2_query_ext_ctrl, minimum) == 40);
    assert!(offset_of!(v4l2_query_ext_ctrl, dims) == 88);
    assert!(size_of::<v4l2_querymenu>() == 44);
    assert!(size_of::<v4l2_ext_control>() == 20);
    assert!(size_of::<v4l2_selection>() == 64);
    assert!(size_of::<v4l2_event_subscription>() == 32);
};

// Layouts with pointers, `long` or `timeval`: checked on 64-bit (x86_64, aarch64), where the
// values were confirmed against the C headers.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<v4l2_format>() == 208);
    assert!(offset_of!(v4l2_format, fmt) == 8);
    assert!(size_of::<v4l2_plane>() == 64);
    assert!(offset_of!(v4l2_plane, m) == 8);
    assert!(offset_of!(v4l2_plane, data_offset) == 16);
    assert!(size_of::<v4l2_buffer>() == 88);
    assert!(offset_of!(v4l2_buffer, timestamp) == 24);
    assert!(offset_of!(v4l2_buffer, sequence) == 56);
    assert!(offset_of!(v4l2_buffer, m) == 64);
    assert!(offset_of!(v4l2_buffer, length) == 72);
    assert!(offset_of!(v4l2_buffer, request_fd) == 80);
    assert!(size_of::<v4l2_ext_controls>() == 32);
    assert!(offset_of!(v4l2_ext_controls, controls) == 24);
    assert!(size_of::<v4l2_event>() == 136);
    assert!(offset_of!(v4l2_event, u) == 8);
    assert!(offset_of!(v4l2_event, pending) == 72);
    assert!(offset_of!(v4l2_event, timestamp) == 80);
    assert!(offset_of!(v4l2_event, id) == 96);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_headers() {
        // Values printed by a C program including linux/videodev2.h on x86_64 and aarch64.
        assert_eq!(VIDIOC_QUERYCAP.nr, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT.nr, 0xc040_5602);
        assert_eq!(VIDIOC_S_FMT.nr, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS.nr, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF.nr, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF.nr, 0xc058_560f);
        assert_eq!(VIDIOC_EXPBUF.nr, 0xc040_5610);
        assert_eq!(VIDIOC_DQBUF.nr, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON.nr, 0x4004_5612);
        assert_eq!(VIDIOC_G_PARM.nr, 0xc0cc_5615);
        assert_eq!(VIDIOC_QUERYMENU.nr, 0xc02c_5625);
        assert_eq!(VIDIOC_G_EXT_CTRLS.nr, 0xc020_5647);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES.nr, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS.nr, 0xc034_564b);
        assert_eq!(VIDIOC_DQEVENT.nr, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT.nr, 0x4020_565a);
        assert_eq!(VIDIOC_G_SELECTION.nr, 0xc040_565e);
        assert_eq!(VIDIOC_QUERY_EXT_CTRL.nr, 0xc0e8_5667);
    }
}

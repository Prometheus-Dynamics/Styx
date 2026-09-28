//! The V4L2 subdevice uAPI structures and ioctl numbers (linux/v4l2-subdev.h,
//! linux/v4l2-mediabus.h), written by hand.

#![allow(non_camel_case_types)]

use std::mem::offset_of;

use crate::geometry::{Fraction, Rect};
use crate::ioctl::{ioctls, ior, iowr};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_mbus_framefmt {
    pub width: u32,
    pub height: u32,
    pub code: u32,
    pub field: u32,
    pub colorspace: u32,
    pub ycbcr_enc: u16,
    pub quantization: u16,
    pub xfer_func: u16,
    pub flags: u16,
    pub reserved: [u16; 10],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_format {
    pub which: u32,
    pub pad: u32,
    pub format: v4l2_mbus_framefmt,
    pub stream: u32,
    pub reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_mbus_code_enum {
    pub pad: u32,
    pub index: u32,
    pub code: u32,
    pub which: u32,
    pub flags: u32,
    pub stream: u32,
    pub reserved: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_frame_size_enum {
    pub index: u32,
    pub pad: u32,
    pub code: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
    pub which: u32,
    pub stream: u32,
    pub reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_frame_interval {
    pub pad: u32,
    pub interval: Fraction,
    pub stream: u32,
    pub which: u32,
    pub reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_frame_interval_enum {
    pub index: u32,
    pub pad: u32,
    pub code: u32,
    pub width: u32,
    pub height: u32,
    pub interval: Fraction,
    pub which: u32,
    pub stream: u32,
    pub reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_selection {
    pub which: u32,
    pub pad: u32,
    pub target: u32,
    pub flags: u32,
    pub r: Rect,
    pub stream: u32,
    pub reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_capability {
    pub version: u32,
    pub capabilities: u32,
    pub reserved: [u32; 14],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_route {
    pub sink_pad: u32,
    pub sink_stream: u32,
    pub source_pad: u32,
    pub source_stream: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_routing {
    pub which: u32,
    pub len_routes: u32,
    pub routes: u64,
    pub num_routes: u32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct v4l2_subdev_client_capability {
    pub capabilities: u64,
}

ioctls! {
    VIDIOC_SUBDEV_QUERYCAP = ior::<v4l2_subdev_capability>(b'V', 0);
    VIDIOC_SUBDEV_ENUM_MBUS_CODE = iowr::<v4l2_subdev_mbus_code_enum>(b'V', 2);
    VIDIOC_SUBDEV_G_FMT = iowr::<v4l2_subdev_format>(b'V', 4);
    VIDIOC_SUBDEV_S_FMT = iowr::<v4l2_subdev_format>(b'V', 5);
    VIDIOC_SUBDEV_G_FRAME_INTERVAL = iowr::<v4l2_subdev_frame_interval>(b'V', 21);
    VIDIOC_SUBDEV_S_FRAME_INTERVAL = iowr::<v4l2_subdev_frame_interval>(b'V', 22);
    VIDIOC_SUBDEV_G_ROUTING = iowr::<v4l2_subdev_routing>(b'V', 38);
    VIDIOC_SUBDEV_S_ROUTING = iowr::<v4l2_subdev_routing>(b'V', 39);
    VIDIOC_SUBDEV_G_SELECTION = iowr::<v4l2_subdev_selection>(b'V', 61);
    VIDIOC_SUBDEV_S_SELECTION = iowr::<v4l2_subdev_selection>(b'V', 62);
    VIDIOC_SUBDEV_ENUM_FRAME_SIZE = iowr::<v4l2_subdev_frame_size_enum>(b'V', 74);
    VIDIOC_SUBDEV_ENUM_FRAME_INTERVAL = iowr::<v4l2_subdev_frame_interval_enum>(b'V', 75);
    VIDIOC_SUBDEV_G_CLIENT_CAP = ior::<v4l2_subdev_client_capability>(b'V', 101);
    VIDIOC_SUBDEV_S_CLIENT_CAP = iowr::<v4l2_subdev_client_capability>(b'V', 102);
}

const _: () = {
    assert!(size_of::<v4l2_mbus_framefmt>() == 48);
    assert!(size_of::<v4l2_subdev_format>() == 88);
    assert!(size_of::<v4l2_subdev_mbus_code_enum>() == 48);
    assert!(size_of::<v4l2_subdev_frame_size_enum>() == 64);
    assert!(size_of::<v4l2_subdev_frame_interval>() == 48);
    assert!(size_of::<v4l2_subdev_frame_interval_enum>() == 64);
    assert!(size_of::<v4l2_subdev_selection>() == 64);
    assert!(size_of::<v4l2_subdev_capability>() == 64);
    assert!(size_of::<v4l2_subdev_route>() == 40);
    assert!(size_of::<v4l2_subdev_routing>() == 64);
    assert!(offset_of!(v4l2_subdev_routing, routes) == 8);
    assert!(offset_of!(v4l2_subdev_routing, num_routes) == 16);
    assert!(size_of::<v4l2_subdev_client_capability>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_headers() {
        assert_eq!(VIDIOC_SUBDEV_QUERYCAP.nr, 0x8040_5600);
        assert_eq!(VIDIOC_SUBDEV_G_FMT.nr, 0xc058_5604);
        assert_eq!(VIDIOC_SUBDEV_ENUM_MBUS_CODE.nr, 0xc030_5602);
        assert_eq!(VIDIOC_SUBDEV_ENUM_FRAME_SIZE.nr, 0xc040_564a);
        assert_eq!(VIDIOC_SUBDEV_ENUM_FRAME_INTERVAL.nr, 0xc040_564b);
        assert_eq!(VIDIOC_SUBDEV_G_FRAME_INTERVAL.nr, 0xc030_5615);
        assert_eq!(VIDIOC_SUBDEV_G_SELECTION.nr, 0xc040_563d);
        assert_eq!(VIDIOC_SUBDEV_G_ROUTING.nr, 0xc040_5626);
        assert_eq!(VIDIOC_SUBDEV_S_CLIENT_CAP.nr, 0xc008_5666);
    }
}

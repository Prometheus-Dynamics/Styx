//! The V4L2 and Styx bridge ABI the bridge client uses: struct layouts, ioctl numbers, control
//! ids and the stream event payload, mirroring `kernel-modules/styx-sensor-bridge/
//! styx_sensor_bridge.h` and `linux/videodev2.h`.

use super::ioctl::{ior, iow, iowr};

// ---- Styx bridge ABI (styx_sensor_bridge.h) --------------------------------------------------

/// `STYX_BRIDGE_PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: u32 = 1;
/// `V4L2_EVENT_PRIVATE_START`.
pub const V4L2_EVENT_PRIVATE_START: u32 = 0x0800_0000;
/// `STYX_BRIDGE_EVENT_STREAM`.
pub const EVENT_STREAM: u32 = V4L2_EVENT_PRIVATE_START + 0x5354;
/// `STYX_BRIDGE_ACTION_START`.
pub const ACTION_START: u32 = 1;
/// `STYX_BRIDGE_ACTION_STOP`.
pub const ACTION_STOP: u32 = 2;
/// `STYX_BRIDGE_FLAG_CONTINUOUS_CLOCK`.
pub const FLAG_CONTINUOUS_CLOCK: u32 = 1;

/// `V4L2_CID_USER_BASE`.
pub const V4L2_CID_USER_BASE: u32 = 0x0098_0900;
/// `STYX_CID_BASE`.
pub const STYX_CID_BASE: u32 = V4L2_CID_USER_BASE + 0x1f00;
/// `STYX_CID_STREAM_ACK` (integer64, execute on write).
pub const CID_STREAM_ACK: u32 = STYX_CID_BASE;
/// `STYX_CID_STREAM_STATE` (read-only, volatile).
pub const CID_STREAM_STATE: u32 = STYX_CID_BASE + 1;
/// `STYX_CID_ACK_TIMEOUT_MS`.
pub const CID_ACK_TIMEOUT_MS: u32 = STYX_CID_BASE + 2;
/// `STYX_CID_POWER`.
pub const CID_POWER: u32 = STYX_CID_BASE + 3;
/// `STYX_CID_STREAM_SEQUENCE` (read-only, volatile).
pub const CID_STREAM_SEQUENCE: u32 = STYX_CID_BASE + 4;

/// `V4L2_CID_VBLANK`.
pub const V4L2_CID_VBLANK: u32 = 0x009e_0901;
/// `V4L2_CID_HBLANK`.
pub const V4L2_CID_HBLANK: u32 = 0x009e_0902;
/// `V4L2_CID_LINK_FREQ`.
pub const V4L2_CID_LINK_FREQ: u32 = 0x009f_0901;
/// `V4L2_CID_PIXEL_RATE`.
pub const V4L2_CID_PIXEL_RATE: u32 = 0x009f_0902;

/// `STYX_BRIDGE_ACK(seq, status)`.
pub const fn ack_value(sequence: u32, status: u32) -> i64 {
    (((status as u64) << 32) | sequence as u64) as i64
}

// ---- V4L2 structs ----------------------------------------------------------------------------

/// `V4L2_SUBDEV_FORMAT_ACTIVE`.
pub const V4L2_SUBDEV_FORMAT_ACTIVE: u32 = 1;
/// `V4L2_CTRL_WHICH_CUR_VAL`.
pub const V4L2_CTRL_WHICH_CUR_VAL: u32 = 0;

/// `struct v4l2_mbus_framefmt`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2MbusFramefmt {
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

/// `struct v4l2_subdev_format`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2SubdevFormat {
    pub which: u32,
    pub pad: u32,
    pub format: V4l2MbusFramefmt,
    pub stream: u32,
    pub reserved: [u32; 7],
}

/// `struct v4l2_event_subscription`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2EventSubscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

/// `struct v4l2_event`; the union is 64 bytes with 8-byte alignment.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct V4l2Event {
    pub type_: u32,
    pub u: V4l2EventUnion,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: libc::timespec,
    pub id: u32,
    pub reserved: [u32; 8],
}

/// The 64-byte payload union of `struct v4l2_event`.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub struct V4l2EventUnion {
    pub data: [u8; 64],
}

/// `struct v4l2_ext_control` (packed); the value union is kept as raw bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2ExtControl {
    pub id: u32,
    pub size: u32,
    pub reserved2: u32,
    pub value: [u8; 8],
}

impl V4l2ExtControl {
    /// A 32-bit (integer, boolean, menu) control.
    pub fn new_i32(id: u32, value: i32) -> Self {
        let mut v = [0u8; 8];
        v[..4].copy_from_slice(&value.to_ne_bytes());
        Self {
            id,
            size: 0,
            reserved2: 0,
            value: v,
        }
    }

    /// A 64-bit control.
    pub fn new_i64(id: u32, value: i64) -> Self {
        Self {
            id,
            size: 0,
            reserved2: 0,
            value: value.to_ne_bytes(),
        }
    }

    /// The value as a 32-bit control.
    pub fn value_i32(&self) -> i32 {
        let v = self.value;
        i32::from_ne_bytes([v[0], v[1], v[2], v[3]])
    }

    /// The value as a 64-bit control.
    #[cfg(test)]
    pub fn value_i64(&self) -> i64 {
        i64::from_ne_bytes(self.value)
    }
}

/// `struct v4l2_ext_controls`.
#[repr(C)]
#[derive(Debug)]
pub struct V4l2ExtControls {
    pub which: u32,
    pub count: u32,
    pub error_idx: u32,
    pub request_fd: i32,
    pub reserved: [u32; 1],
    pub controls: *mut V4l2ExtControl,
}

/// `struct v4l2_queryctrl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Queryctrl {
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

/// `struct v4l2_querymenu` (packed); for integer menus the union holds an `s64`.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Querymenu {
    pub id: u32,
    pub index: u32,
    pub value: [u8; 32],
    pub reserved: u32,
}

pub const VIDIOC_SUBDEV_G_FMT: u32 = iowr::<V4l2SubdevFormat>(b'V', 4);
pub const VIDIOC_SUBDEV_S_FMT: u32 = iowr::<V4l2SubdevFormat>(b'V', 5);
pub const VIDIOC_QUERYCTRL: u32 = iowr::<V4l2Queryctrl>(b'V', 36);
pub const VIDIOC_QUERYMENU: u32 = iowr::<V4l2Querymenu>(b'V', 37);
pub const VIDIOC_G_EXT_CTRLS: u32 = iowr::<V4l2ExtControls>(b'V', 71);
pub const VIDIOC_S_EXT_CTRLS: u32 = iowr::<V4l2ExtControls>(b'V', 72);
pub const VIDIOC_DQEVENT: u32 = ior::<V4l2Event>(b'V', 89);
pub const VIDIOC_SUBSCRIBE_EVENT: u32 = iow::<V4l2EventSubscription>(b'V', 90);
pub const VIDIOC_UNSUBSCRIBE_EVENT: u32 = iow::<V4l2EventSubscription>(b'V', 91);

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn struct_layouts_match_the_kernel() {
        assert_eq!(size_of::<V4l2MbusFramefmt>(), 48);
        assert_eq!(size_of::<V4l2SubdevFormat>(), 88);
        assert_eq!(size_of::<V4l2EventSubscription>(), 32);
        assert_eq!(size_of::<V4l2ExtControl>(), 20);
        assert_eq!(size_of::<V4l2Queryctrl>(), 68);
        assert_eq!(size_of::<V4l2Querymenu>(), 44);
        assert_eq!(offset_of!(V4l2Event, u), 8);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<V4l2ExtControls>(), 32);
            assert_eq!(size_of::<V4l2Event>(), 136);
            assert_eq!(offset_of!(V4l2Event, id), 96);
            assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
            assert_eq!(VIDIOC_S_EXT_CTRLS, 0xc020_5648);
        }
        assert_eq!(VIDIOC_SUBDEV_S_FMT, 0xc058_5605);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
        assert_eq!(VIDIOC_QUERYMENU, 0xc02c_5625);
    }

    #[test]
    fn styx_ids_match_the_header() {
        assert_eq!(EVENT_STREAM, 0x0800_5354);
        assert_eq!(CID_STREAM_ACK, 0x0098_2800);
        assert_eq!(CID_STREAM_SEQUENCE, 0x0098_2804);
        assert_eq!(ack_value(7, 0), 7);
        assert_eq!(ack_value(7, 5), (5i64 << 32) | 7);
        assert_eq!(ack_value(u32::MAX, u32::MAX), -1);
    }

    #[test]
    fn ext_control_values_round_trip() {
        assert_eq!(V4l2ExtControl::new_i32(1, -5).value_i32(), -5);
        assert_eq!(
            V4l2ExtControl::new_i64(1, 400_000_000_000).value_i64(),
            400_000_000_000
        );
    }
}

//! The media controller uAPI structures and ioctl numbers (linux/media.h), written by hand.

#![allow(non_camel_case_types)]

use std::mem::offset_of;

use crate::ioctl::{io, ioctls, ior, iowr};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct media_device_info {
    pub driver: [u8; 16],
    pub model: [u8; 32],
    pub serial: [u8; 40],
    pub bus_info: [u8; 32],
    pub media_version: u32,
    pub hw_revision: u32,
    pub driver_version: u32,
    pub reserved: [u32; 31],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct media_v2_entity {
    pub id: u32,
    pub name: [u8; 64],
    pub function: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

/// `struct media_v2_interface`; for device nodes `raw[0..2]` is the devnode major/minor.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct media_v2_interface {
    pub id: u32,
    pub intf_type: u32,
    pub flags: u32,
    pub reserved: [u32; 9],
    pub raw: [u32; 16],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct media_v2_pad {
    pub id: u32,
    pub entity_id: u32,
    pub flags: u32,
    pub index: u32,
    pub reserved: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct media_v2_link {
    pub id: u32,
    pub source_id: u32,
    pub sink_id: u32,
    pub flags: u32,
    pub reserved: [u32; 6],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct media_v2_topology {
    pub topology_version: u64,
    pub num_entities: u32,
    pub reserved1: u32,
    pub ptr_entities: u64,
    pub num_interfaces: u32,
    pub reserved2: u32,
    pub ptr_interfaces: u64,
    pub num_pads: u32,
    pub reserved3: u32,
    pub ptr_pads: u64,
    pub num_links: u32,
    pub reserved4: u32,
    pub ptr_links: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct media_pad_desc {
    pub entity: u32,
    pub index: u16,
    pub flags: u32,
    pub reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct media_link_desc {
    pub source: media_pad_desc,
    pub sink: media_pad_desc,
    pub flags: u32,
    pub reserved: [u32; 2],
}

ioctls! {
    MEDIA_IOC_DEVICE_INFO = iowr::<media_device_info>(b'|', 0x00);
    MEDIA_IOC_SETUP_LINK = iowr::<media_link_desc>(b'|', 0x03);
    MEDIA_IOC_G_TOPOLOGY = iowr::<media_v2_topology>(b'|', 0x04);
    MEDIA_IOC_REQUEST_ALLOC = ior::<libc::c_int>(b'|', 0x05);
    MEDIA_REQUEST_IOC_QUEUE = io(b'|', 0x80);
    MEDIA_REQUEST_IOC_REINIT = io(b'|', 0x81);
}

const _: () = {
    assert!(size_of::<media_device_info>() == 256);
    assert!(size_of::<media_v2_entity>() == 96);
    assert!(size_of::<media_v2_interface>() == 112);
    assert!(size_of::<media_v2_pad>() == 32);
    assert!(size_of::<media_v2_link>() == 40);
    assert!(size_of::<media_v2_topology>() == 72);
    assert!(offset_of!(media_v2_topology, ptr_links) == 64);
    assert!(size_of::<media_pad_desc>() == 20);
    assert!(size_of::<media_link_desc>() == 52);
    assert!(offset_of!(media_link_desc, sink) == 20);
    assert!(offset_of!(media_link_desc, flags) == 40);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_headers() {
        assert_eq!(MEDIA_IOC_DEVICE_INFO.nr, 0xc100_7c00);
        assert_eq!(MEDIA_IOC_SETUP_LINK.nr, 0xc034_7c03);
        assert_eq!(MEDIA_IOC_G_TOPOLOGY.nr, 0xc048_7c04);
        assert_eq!(MEDIA_IOC_REQUEST_ALLOC.nr, 0x8004_7c05);
        assert_eq!(MEDIA_REQUEST_IOC_QUEUE.nr, 0x7c80);
        assert_eq!(MEDIA_REQUEST_IOC_REINIT.nr, 0x7c81);
    }
}

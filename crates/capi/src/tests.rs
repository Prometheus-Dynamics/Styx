use std::time::Duration;

use lemnos_abi::{AbiVersion, Handle, Status};
use styx::prelude::*;

use crate::surface::{self, FrameInfo, HEADER_LAYOUT_HASH, LAYOUT, VERSION};
use crate::{Camera, ffi};

fn virtual_camera(name: &str) -> u32 {
    let device = VirtualSourceConfig::new()
        .name(name)
        .format(FourCc::RG24)
        .resolution(320, 180)
        .fps(60)
        .into_device();
    surface::add_camera(device)
}

#[test]
fn the_header_states_this_layout() {
    let header = include_str!("../include/styx.h");
    let line = header
        .lines()
        .find_map(|l| l.strip_prefix("#define STYX_LAYOUT_HASH "))
        .expect("STYX_LAYOUT_HASH");
    let hex = line.trim().trim_start_matches("0x").trim_end_matches("ULL");
    assert_eq!(u64::from_str_radix(hex, 16).unwrap(), LAYOUT);
    assert_eq!(HEADER_LAYOUT_HASH, LAYOUT);
    assert_eq!(size_of::<FrameInfo>(), 48);
}

#[test]
fn another_major_is_refused() {
    let caller = AbiVersion::new(VERSION.major + 1, 0, LAYOUT);
    // SAFETY: a valid version for the call.
    let status = unsafe { ffi::styx_check_version(&caller) };
    assert_eq!(status, Status::VERSION_MISMATCH);
    // SAFETY: as above.
    assert_eq!(unsafe { ffi::styx_check_version(&VERSION) }, Status::OK);
}

#[test]
fn frames_come_through_the_surface_and_go_back() {
    let camera = Camera::add_virtual("capi-frames", FourCc::RG24.to_u32(), 320, 180, 60).unwrap();
    assert_eq!(camera.name(), "capi-frames");
    let stream = camera.open(FourCc::RG24.to_u32(), 320, 180, 0).unwrap();
    let mut got = 0;
    for _ in 0..50 {
        let Some(frame) = stream.next(Duration::from_millis(200)).unwrap() else {
            continue;
        };
        let info = *frame.info();
        assert_eq!((info.width, info.height), (320, 180));
        assert_eq!(info.fourcc, FourCc::RG24.to_u32());
        assert_eq!(info.sequence, got);
        let (bytes, stride) = frame.plane(0).unwrap();
        assert!(stride >= 320 * 3);
        assert!(bytes.len() >= stride * 179 + 320 * 3);
        got += 1;
        if got == 5 {
            break;
        }
    }
    assert_eq!(got, 5);
}

#[test]
fn a_released_frame_handle_is_refused() {
    let index = virtual_camera("capi-stale");
    let request = surface::StreamRequest {
        struct_size: size_of::<surface::StreamRequest>() as u32,
        ..Default::default()
    };
    let mut stream = Handle::NULL;
    // SAFETY: valid pointers for the call.
    let status = unsafe {
        ffi::styx_stream_open(index, &request, &mut stream, lemnos_abi::MessageSink::NONE)
    };
    assert_eq!(status, Status::OK);
    let mut frame = Handle::NULL;
    let mut status = Status::TIMED_OUT;
    for _ in 0..20 {
        // SAFETY: valid pointers; info may be null.
        status = unsafe { ffi::styx_stream_next(stream, 200, &mut frame, core::ptr::null_mut()) };
        if status == Status::OK {
            break;
        }
    }
    assert_eq!(status, Status::OK);
    assert_eq!(ffi::styx_frame_release(frame), Status::OK);
    assert_eq!(ffi::styx_frame_release(frame), Status::STALE_HANDLE);
    assert_eq!(ffi::styx_stream_close(stream), Status::OK);
    assert_eq!(ffi::styx_stream_close(stream), Status::STALE_HANDLE);
    // An unknown camera.
    // SAFETY: valid pointers for the call.
    let status = unsafe {
        ffi::styx_stream_open(9999, &request, &mut stream, lemnos_abi::MessageSink::NONE)
    };
    assert_eq!(status, Status::NOT_FOUND);
}

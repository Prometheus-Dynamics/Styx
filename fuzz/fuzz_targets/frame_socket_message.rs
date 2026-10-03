//! Frame socket messages (`styx-frame-lease-v1`: JSON with the frame descriptor and its
//! backing) as a consumer imports them, over memfds standing in for the descriptors the server
//! sends, then every plane read. A message must never make the consumer read outside what it
//! was sent.
//!
//! Input: a byte with the descriptor count and sizes, then the JSON.

#![no_main]

use std::os::fd::{FromRawFd, OwnedFd};

use libfuzzer_sys::fuzz_target;

fn memfd(len: usize) -> OwnedFd {
    // SAFETY: plain syscalls; the descriptor is owned from here.
    unsafe {
        let fd = libc::memfd_create(c"fuzz".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0, "memfd_create");
        assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
        OwnedFd::from_raw_fd(fd)
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&shape, json)) = data.split_first() else {
        return;
    };
    // Up to 4 descriptors of 0, 1, 4096 or 65536 bytes.
    let count = usize::from(shape & 3) + 1;
    let len = [0, 1, 4096, 65536][usize::from(shape >> 2 & 3)];
    let fds = (0..count).map(|_| memfd(len)).collect();
    styx::ipc::frame_socket::fuzz_import(json, fds);
});

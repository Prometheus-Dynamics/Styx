//! Capture helpers against a real device: read-only dma-buf export and waiting for filled
//! buffers. Opt-in: runs only when `STYX_KERNEL_STREAM_DEVICE=/dev/videoN` names a
//! single-planar capture device that is free to use.

#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd;
use std::time::Duration;

use styx_kernel::v4l2::{BufType, DmaBufAccess, Memory, QueueBuffer, VideoDevice};

fn access_mode(fd: &impl AsRawFd) -> i32 {
    // SAFETY: F_GETFL on a valid descriptor has no memory effects.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "fcntl: {}", std::io::Error::last_os_error());
    flags & libc::O_ACCMODE
}

#[test]
fn exports_read_only_and_waits_for_frames() {
    let Ok(path) = std::env::var("STYX_KERNEL_STREAM_DEVICE") else {
        eprintln!("STYX_KERNEL_STREAM_DEVICE not set; skipping");
        return;
    };
    let dev = VideoDevice::open(&path).expect("open");
    let ty = BufType::VideoCapture;
    let bufs = dev.request_buffers(ty, Memory::Mmap, 3).expect("reqbufs");
    assert!(bufs.count >= 1);

    let ro = dev
        .export_buffer_with(ty, 0, 0, DmaBufAccess::ReadOnly)
        .expect("expbuf read-only");
    assert_eq!(access_mode(&ro), libc::O_RDONLY);
    let rw = dev
        .export_buffer_with(ty, 0, 0, DmaBufAccess::ReadWrite)
        .expect("expbuf read-write");
    assert_eq!(access_mode(&rw), libc::O_RDWR);
    drop((ro, rw));

    for i in 0..bufs.count {
        dev.queue(&QueueBuffer::mmap(ty, i)).expect("qbuf");
    }
    dev.stream_on(ty).expect("streamon");
    let mut sequences = Vec::new();
    while sequences.len() < 3 {
        let ready = dev
            .wait_readable(Some(Duration::from_secs(3)))
            .expect("poll");
        assert!(ready.readable, "timed out waiting for a frame: {ready:?}");
        assert!(!ready.priority && !ready.writable);
        if let Some(buf) = dev.dequeue(ty, Memory::Mmap).expect("dqbuf") {
            sequences.push(buf.sequence);
            dev.queue(&QueueBuffer::mmap(ty, buf.index))
                .expect("requeue");
        }
    }
    dev.stream_off(ty).expect("streamoff");
    dev.free_buffers(ty, Memory::Mmap).expect("free");
    assert!(sequences.windows(2).all(|w| w[1] > w[0]), "{sequences:?}");
}

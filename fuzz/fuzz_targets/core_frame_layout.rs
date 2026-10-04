//! Frames whose layout came from somewhere else: a frame descriptor (sizes, offsets, strides,
//! format) as another process sends it, over a memfd, dma-buf-like descriptors or host
//! buffers, then every way of reading, copying, cropping and re-allocating it. Nothing may
//! read outside the backing, panic or allocate from an absurd plane length.

#![no_main]

use std::os::fd::{FromRawFd, OwnedFd};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smallvec::SmallVec;
use styx_core::prelude::*;
use styx_core::requirements::FrameRect;

const FOURCCS: [FourCc; 26] = [
    FourCc::R8,
    FourCc::GREY,
    FourCc::RG24,
    FourCc::BG24,
    FourCc::RGBA,
    FourCc::BGRA,
    FourCc::NV12,
    FourCc::NV21,
    FourCc::YUYV,
    FourCc::UYVY,
    FourCc::I420,
    FourCc::YV12,
    FourCc::D32F,
    FourCc::R16,
    FourCc::XR24,
    FourCc::RG48,
    FourCc::MJPG,
    FourCc::H264,
    FourCc::RGGB,
    FourCc::BGGR,
    FourCc::new(*b"pRAA"),
    FourCc::new(*b"RG10"),
    FourCc::new(*b"Y10P"),
    FourCc::new(*b"NV16"),
    FourCc::new(*b"YU12"),
    FourCc::new(*b"BA81"),
];

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Num {
    Small(u16),
    Any(u64),
}

impl Num {
    fn get(self) -> usize {
        match self {
            Num::Small(v) => usize::from(v),
            Num::Any(v) => v as usize,
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Backing {
    /// One memfd of `size` (0..64 KiB) for all planes.
    Memfd(u16),
    /// One memfd per plane, as dma-bufs are sent, at the given offsets.
    Planes(Vec<(u16, Num)>),
    /// Host buffers sized to the layout (when that is small).
    Host,
}

#[derive(Arbitrary, Debug)]
struct Input {
    fourcc: u8,
    raw_fourcc: Option<[u8; 4]>,
    width: u16,
    height: u16,
    planes: Vec<(Num, Num, Num)>,
    backing: Backing,
    crop: (u16, u16, u16, u16),
    pyramid: u8,
    align: u8,
}

fn memfd(len: usize) -> OwnedFd {
    // SAFETY: plain syscalls; the descriptor is owned from here.
    unsafe {
        let fd = libc::memfd_create(c"fuzz".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0, "memfd_create");
        assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
        OwnedFd::from_raw_fd(fd)
    }
}

fn read_all(frame: &FrameLease) {
    let _ = frame.validate_plane_layouts();
    let _ = (frame.payload_bytes(), frame.visible_payload_bytes());
    let _ = (
        frame.is_tightly_packed(),
        frame.first_plane_visible_row_bytes(),
    );
    for p in frame.planes() {
        std::hint::black_box(p.data().iter().fold(0u8, |a, b| a ^ b));
    }
    for i in 0..frame.layouts().len() {
        let _ = frame.plane_shape(i);
        let _ = frame.try_as_contiguous_visible_plane(i);
        if let Ok(rows) = frame.visible_rows(i) {
            for row in rows {
                std::hint::black_box(row.data().iter().fold(0u8, |a, b| a ^ b));
            }
        }
    }
    if let Ok(rows) = frame.luma_rows() {
        for row in rows {
            std::hint::black_box(row.data().first());
        }
    }
    if let Ok(v) = frame.to_visible_vec() {
        let mut copy = vec![0; v.len()];
        let _ = frame.copy_visible_to_slice(&mut copy);
    }
}

fuzz_target!(|input: Input| {
    let fourcc = match input.raw_fourcc {
        Some(b) => FourCc::new(b),
        None => FOURCCS[usize::from(input.fourcc) % FOURCCS.len()],
    };
    let planes: SmallVec<[FramePlaneDescriptor; 4]> = input
        .planes
        .iter()
        .take(4)
        .map(|&(offset, len, stride)| FramePlaneDescriptor {
            offset: offset.get(),
            len: len.get(),
            stride: stride.get(),
        })
        .collect();
    let descriptor = FrameLeaseDescriptor {
        width: input.width.into(),
        height: input.height.into(),
        fourcc,
        timestamp: 0,
        color: ColorSpace::Srgb,
        planes,
    };
    let frame = match input.backing {
        Backing::Memfd(size) => {
            FrameLease::from_memfd_import(descriptor, memfd(usize::from(size))).ok()
        }
        Backing::Planes(fds) => {
            let planes = fds
                .into_iter()
                .take(4)
                .map(|(size, offset)| FrameFdPlane {
                    fd: memfd(usize::from(size)),
                    offset: offset.get(),
                    len: usize::from(size),
                })
                .collect();
            FrameLease::from_dmabuf_import(descriptor, planes).ok()
        }
        Backing::Host => {
            let Some(meta) = descriptor.to_meta() else {
                return;
            };
            let layouts = descriptor.layouts();
            if layouts
                .iter()
                .any(|l| l.offset.checked_add(l.len).is_none_or(|e| e > 1 << 20))
            {
                return;
            }
            let buffers = layouts
                .iter()
                .map(|l| {
                    let mut b = BufferPool::with_limits(1, (l.offset + l.len).max(1), 0).lease();
                    b.resize(l.offset + l.len);
                    b
                })
                .collect();
            Some(FrameLease::multi_plane(meta, buffers, layouts))
        }
    };
    let Some(frame) = frame else {
        return;
    };
    // Shared views of one frame for the consuming conversions below.
    let frame = frame.into_shareable();
    let Some(view) = frame.share() else {
        return;
    };
    read_all(&view);
    read_all(&frame);
    let owned = frame.materialize_owned();
    read_all(&owned);
    // Allocating a frame like this one sizes it from the format: only for frames whose planes
    // hold that format (imports are checked; host frames here are built by the harness).
    if frame.validate_plane_layouts().is_ok()
        && let Ok(same) = frame.allocate_same_layout()
    {
        read_all(&same);
    }
    let (x, y, w, h) = input.crop;
    if let Ok(view) = view.crop_view(FrameRect::new(x.into(), y.into(), w.into(), h.into())) {
        read_all(&view);
    }
    if let Some(f) = frame.share()
        && let Ok(p) = f.with_box_pyramid(input.pyramid % 4, usize::from(input.align))
    {
        for level in 1..4 {
            if let Some(l) = p.pyramid_level(level) {
                read_all(l);
            }
        }
    }
    if let Ok(luma) = frame.into_luma() {
        read_all(&luma);
    }
});

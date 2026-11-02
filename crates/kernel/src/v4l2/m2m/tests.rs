use super::*;
use crate::FourCc;
use crate::v4l2::{CapabilityFlags, PixFormat};

/// The first node whose driver is `driver` (a test driver loaded with `modprobe`).
fn node(driver: &str) -> Option<VideoDevice> {
    crate::v4l2::list_video_nodes()
        .into_iter()
        .filter_map(|p| VideoDevice::open(p).ok())
        .find(|d| {
            let caps = d.capabilities();
            caps.driver == driver
                && caps
                    .device_caps
                    .intersects(CapabilityFlags::VIDEO_M2M | CapabilityFlags::VIDEO_M2M_MPLANE)
        })
}

fn pix(width: u32, height: u32, fourcc: FourCc) -> Format {
    Format::Single(PixFormat {
        width,
        height,
        fourcc,
        ..Default::default()
    })
}

#[test]
fn colour_planes_follow_each_other_in_one_buffer() {
    let nv12 = Format::Single(PixFormat {
        width: 100,
        height: 10,
        bytes_per_line: 128,
        ..Default::default()
    });
    let s = spans(&nv12, &[(100, 10), (100, 5)]);
    assert_eq!(
        s,
        [
            Span {
                buffer: 0,
                offset: 0,
                stride: 128
            },
            Span {
                buffer: 0,
                offset: 1280,
                stride: 128
            },
        ]
    );
    // YU12: the chroma planes at half the luma stride.
    let s = spans(&nv12, &[(100, 10), (50, 5), (50, 5)]);
    assert_eq!(
        s[1],
        Span {
            buffer: 0,
            offset: 1280,
            stride: 64
        }
    );
    assert_eq!(
        s[2],
        Span {
            buffer: 0,
            offset: 1600,
            stride: 64
        }
    );
}

#[test]
fn colour_planes_take_a_buffer_each_when_there_are_enough() {
    let multi = Format::Multi(crate::v4l2::PixFormatMplane {
        width: 100,
        height: 10,
        planes: vec![
            crate::v4l2::PlaneFormat {
                bytes_per_line: 112,
                size_image: 1120,
            },
            crate::v4l2::PlaneFormat {
                bytes_per_line: 0,
                size_image: 560,
            },
        ],
        ..Default::default()
    });
    let s = spans(&multi, &[(100, 10), (100, 5)]);
    assert_eq!(
        s[0],
        Span {
            buffer: 0,
            offset: 0,
            stride: 112
        }
    );
    // A stride the driver left at 0 is the row width.
    assert_eq!(
        s[1],
        Span {
            buffer: 1,
            offset: 0,
            stride: 100
        }
    );
}

#[test]
fn formats_convert_to_lemnos_and_back() {
    assert!(to_lemnos(&Format::Meta(Default::default())).is_err());
    let single = Format::Single(PixFormat {
        width: 2,
        height: 2,
        fourcc: FourCc::new(b"YUYV"),
        field: 1,
        bytes_per_line: 4,
        size_image: 8,
        ..Default::default()
    });
    assert_eq!(from_lemnos(&to_lemnos(&single).unwrap()), single);
    let multi = Format::Multi(crate::v4l2::PixFormatMplane {
        width: 4,
        height: 2,
        fourcc: FourCc::new(b"NM12"),
        planes: vec![
            crate::v4l2::PlaneFormat {
                bytes_per_line: 4,
                size_image: 8,
            },
            crate::v4l2::PlaneFormat {
                bytes_per_line: 4,
                size_image: 4,
            },
        ],
        ..Default::default()
    });
    assert_eq!(from_lemnos(&to_lemnos(&multi).unwrap()), multi);
}

/// vim2m (`modprobe vim2m`): RGB24 frames through the scaler come back the same size,
/// stamped with their source frame's time. Skipped without the module.
#[test]
fn vim2m_round_trips_frames() {
    let Some(dev) = node("vim2m") else {
        eprintln!("skipped: no vim2m node (modprobe vim2m)");
        return;
    };
    let rgb = FourCc::new(b"RGB3");
    let (w, h) = (64, 48);
    let mut s = M2mSession::open(dev, &pix(w, h, rgb), &pix(w, h, rgb), 2).unwrap();
    let frame: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
    for k in 0..4u64 {
        assert!(s.push(&[&frame], Duration::from_millis(k)).unwrap());
        let out = s.pull(Duration::from_secs(2)).unwrap().expect("a frame");
        assert_eq!(out.buffer.timestamp, Duration::from_millis(k));
        assert_eq!(s.data(&out, 0).len(), frame.len());
        s.release(out).unwrap();
    }
    s.close().unwrap();
}

/// vim2m importing dma-bufs from the system heap: each frame is read in place, comes back
/// the same, and the frame held for a slot is dropped once the device gives it back.
/// Skipped without the module or the heap.
#[test]
fn vim2m_reads_dmabufs_in_place() {
    use crate::dma_heap::{Access, DmaHeap};
    use std::rc::Rc;
    let (Some(dev), Ok(heap)) = (node("vim2m"), DmaHeap::open("system")) else {
        eprintln!("skipped: no vim2m node or system dma-heap");
        return;
    };
    let rgb = FourCc::new(b"RGB3");
    let (w, h) = (64u32, 48u32);
    let len = (w * h * 3) as usize;
    let mut s: M2mSession<Rc<()>> =
        M2mSession::open_importing(dev, &pix(w, h, rgb), &pix(w, h, rgb), 2).unwrap();
    assert!(s.imports());
    assert!(s.push(&[&[0u8; 4][..]], Duration::ZERO).is_err());
    let bufs: Vec<_> = (0..2).map(|_| heap.allocate(len).unwrap()).collect();
    let held = Rc::new(());
    for k in 0..6u64 {
        let buf = &bufs[k as usize % 2];
        let mut map = buf.map().unwrap();
        buf.begin_cpu_access(Access::Write).unwrap();
        for (i, b) in map.as_mut_slice()[..len].iter_mut().enumerate() {
            *b = ((i as u64 + k * 7) % 251) as u8;
        }
        buf.end_cpu_access(Access::Write).unwrap();
        let plane = DmabufSource {
            fd: buf.as_fd(),
            offset: 0,
            bytes: len as u32,
        };
        assert!(
            s.push_dmabuf(&[plane], Duration::from_millis(k), held.clone())
                .unwrap()
        );
        let out = s.pull(Duration::from_secs(2)).unwrap().expect("a frame");
        assert_eq!(out.buffer.timestamp, Duration::from_millis(k));
        assert_eq!(s.data(&out, 0), &map.as_slice()[..len], "frame {k}");
        s.release(out).unwrap();
    }
    // Taking the slots back drops what was held for them.
    let _ = s.can_push().unwrap();
    assert!(Rc::strong_count(&held) <= 2, "{}", Rc::strong_count(&held));
    s.close().unwrap();
}

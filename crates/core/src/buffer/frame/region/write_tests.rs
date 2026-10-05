//! Writable regions: frames that write caller-provided memory in place, and the order of the
//! hooks around it. Run under Miri too (the planes are cut from raw pointers).

use alloc::vec::Vec;

use super::*;
use crate::buffer::meta::FrameMutability;
use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
use crate::sync::{Arc, Mutex};
use smallvec::smallvec;

type Log = Arc<Mutex<Vec<&'static str>>>;

/// Records every hook, in order.
struct Recorder(Log);

impl RegionHooks for Recorder {
    fn begin_cpu_read(&self, _bytes: &[u8]) {
        self.0.lock().push("begin_read");
    }
    fn begin_cpu_write(&self, bytes: &[u8]) {
        assert_eq!(bytes.len(), 32);
        self.0.lock().push("begin_write");
    }
    fn end_cpu_write(&self, bytes: &[u8]) {
        assert_eq!(bytes.len(), 32);
        self.0.lock().push("end_write");
    }
    fn release(&self) {
        self.0.lock().push("release");
    }
}

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn events(log: &Log) -> Vec<&'static str> {
    log.lock().clone()
}

fn meta(code: FourCc, w: u32, h: u32) -> FrameMeta {
    let format = MediaFormat::new(code, Resolution::new(w, h).unwrap(), ColorSpace::Unknown);
    FrameMeta::new(format, 9)
}

/// NV12 4x2 in a 32-byte region: Y at 4 (8 bytes), UV at 16 (4 bytes).
fn nv12_layouts() -> SmallVec<[PlaneLayout; 3]> {
    smallvec![
        PlaneLayout {
            offset: 4,
            len: 8,
            stride: 4
        },
        PlaneLayout {
            offset: 16,
            len: 4,
            stride: 4
        }
    ]
}

fn writable(buf: &mut [u8; 32], log: &Log) -> MemoryRegion<Recorder> {
    // SAFETY: every test drops the frame (and so the region) before touching `buf` again.
    unsafe { MemoryRegion::from_raw_mut(buf.as_mut_ptr(), buf.len(), Recorder(log.clone())) }
}

#[test]
fn frames_write_regions_through_their_planes_and_end_writes_before_sharing() {
    let mut buf = [0u8; 32];
    let log = new_log();
    let mut frame = FrameLease::from_region(
        meta(FourCc::NV12, 4, 2),
        nv12_layouts(),
        writable(&mut buf, &log),
    );
    assert!(frame.can_write_planes());
    assert!(frame.require_host_writable().is_ok());
    {
        let mut planes = frame.planes_mut();
        assert_eq!((planes[0].data().len(), planes[1].data().len()), (8, 4));
        planes[0].data().iter_mut().for_each(|b| *b = 1);
        planes[1].data().copy_from_slice(&[2, 3, 4, 5]);
    }
    assert_eq!(events(&log), ["begin_write"]);
    // The CPU reads its own writes: no read invalidate.
    assert_eq!(frame.planes()[1].data(), [2, 3, 4, 5]);
    assert_eq!(events(&log), ["begin_write"]);

    let view = frame.share().expect("region frames share");
    assert_eq!(events(&log), ["begin_write", "end_write"]);
    assert!(!frame.can_write_planes(), "a view is out");
    assert!(frame.plane_data_mut(0).is_none());
    assert!(frame.planes_mut().iter_mut().all(|p| p.data().is_empty()));
    assert_eq!(view.planes()[0].data(), [1; 8]);
    drop(view);

    // The one owner again: a second write window.
    assert!(frame.can_write_planes());
    frame.plane_data_mut(1).unwrap()[0] = 7;
    assert_eq!(events(&log), ["begin_write", "end_write", "begin_write"]);
    drop(frame);
    assert_eq!(
        events(&log),
        [
            "begin_write",
            "end_write",
            "begin_write",
            "end_write",
            "release"
        ]
    );
    let mut expected = [0u8; 32];
    expected[4..12].fill(1);
    expected[16..20].copy_from_slice(&[7, 3, 4, 5]);
    assert_eq!(buf, expected);
}

#[test]
fn visible_rows_and_copies_write_regions() {
    let mut buf = [0u8; 32];
    let log = new_log();
    {
        let layouts = smallvec![PlaneLayout {
            offset: 0,
            len: 32,
            stride: 8
        }];
        let mut frame =
            FrameLease::from_region(meta(FourCc::GREY, 6, 4), layouts, writable(&mut buf, &log));
        let mut rows = frame.visible_rows_mut(0).expect("writable");
        assert_eq!((rows.len(), rows.row_bytes(), rows.stride()), (4, 6, 8));
        rows.for_each_row_mut(|i, mut row| row.data().fill(i as u8 + 1));
        let src: Vec<u8> = (0..24).collect();
        assert_eq!(frame.copy_slice_to_visible_plane(0, &src), Ok(24));
        assert_eq!(frame.planes()[0].data()[8..14], [6, 7, 8, 9, 10, 11]);
        frame.finish_cpu_write();
        assert_eq!(events(&log), ["begin_write", "end_write"]);
        frame.finish_cpu_write();
        assert_eq!(events(&log), ["begin_write", "end_write"], "none open");
    }
    assert_eq!(events(&log), ["begin_write", "end_write", "release"]);
    assert_eq!(buf[24..30], [18, 19, 20, 21, 22, 23]);
    assert_eq!(buf[6..8], [0, 0], "padding untouched");
}

#[test]
fn reads_then_writes_in_place() {
    let mut buf = [5u8; 32];
    let log = new_log();
    {
        let layouts = smallvec![PlaneLayout {
            offset: 0,
            len: 32,
            stride: 8
        }];
        let mut frame =
            FrameLease::from_region(meta(FourCc::GREY, 8, 4), layouts, writable(&mut buf, &log));
        assert_eq!(frame.planes()[0].data()[0], 5);
        for b in frame.planes_mut()[0].data() {
            *b += 1;
        }
    }
    assert_eq!(
        events(&log),
        ["begin_read", "begin_write", "end_write", "release"]
    );
    assert_eq!(buf, [6; 32]);
}

#[test]
fn handles_read_only_meta_and_read_only_regions_block_writes() {
    let mut buf = [0u8; 32];
    let log = new_log();
    {
        let mut frame = FrameLease::from_region(
            meta(FourCc::NV12, 4, 2),
            nv12_layouts(),
            writable(&mut buf, &log),
        );
        frame.plane_data_mut(0).unwrap()[0] = 1;
        let handle = frame.external_backing_handle().unwrap();
        assert_eq!(events(&log), ["begin_write", "end_write"]);
        assert!(!frame.can_write_planes());
        assert!(frame.plane_data_mut(0).is_none());
        drop(handle);
        assert!(frame.can_write_planes());

        frame.meta_mut().mutability = FrameMutability::ReadOnly;
        assert!(!frame.can_write_planes());
        assert!(frame.visible_rows_mut(0).is_err());
    }
    assert_eq!(buf[4], 1);

    static PIXELS: [u8; 16] = [3; 16];
    let layouts = smallvec![PlaneLayout {
        offset: 0,
        len: 16,
        stride: 4
    }];
    let mut frame = FrameLease::from_region(
        meta(FourCc::GREY, 4, 4),
        layouts,
        MemoryRegion::from_static(&PIXELS),
    );
    assert!(!frame.can_write_planes());
    assert!(frame.plane_data_mut(0).is_none());
    assert!(frame.planes_mut()[0].data().is_empty());
    assert!(frame.visible_rows_mut(0).is_err());

    // Writable memory the CPU cannot reach: not writable either.
    let mut buf = [0u8; 32];
    let log = new_log();
    let layouts = smallvec![PlaneLayout {
        offset: 0,
        len: 16,
        stride: 4
    }];
    let mut frame = FrameLease::from_region(
        meta(FourCc::GREY, 4, 4),
        layouts,
        writable(&mut buf, &log).with_cpu_access(CpuAccess::None),
    );
    assert!(!frame.can_write_planes());
    assert!(frame.plane_data_mut(0).is_none());
    drop(frame);
    assert_eq!(events(&log), ["release"]);
}

#[test]
fn overlapping_or_out_of_range_planes_are_not_handed_out() {
    let mut buf = [0u8; 32];
    let log = new_log();
    {
        let layouts = smallvec![
            PlaneLayout {
                offset: 0,
                len: 16,
                stride: 4
            },
            PlaneLayout {
                offset: 8,
                len: 8,
                stride: 4
            },
            PlaneLayout {
                offset: 28,
                len: 8,
                stride: 4
            }
        ];
        let mut frame =
            FrameLease::from_region(meta(FourCc::NV12, 4, 4), layouts, writable(&mut buf, &log));
        let mut planes = frame.planes_mut();
        assert_eq!(planes[0].data().len(), 16);
        assert!(planes[1].data().is_empty(), "overlaps plane 0");
        assert!(planes[2].data().is_empty(), "past the region");
        planes[0].data().fill(9);
    }
    assert_eq!(buf[..16], [9; 16]);
    assert_eq!(buf[16..], [0; 16]);
}

#[test]
fn static_mut_regions_are_writable() {
    // Leaked on purpose: a `'static` buffer, as a firmware's carve-out (Miri: ignore leaks).
    if cfg!(miri) {
        return;
    }
    let bytes: &'static mut [u8] = alloc::vec![0u8; 8].leak();
    let mut region = MemoryRegion::from_static_mut(bytes);
    assert!(region.is_writable());
    region.bytes_mut().unwrap()[3] = 4;
    assert_eq!(region.bytes()[3], 4);
    assert!(!MemoryRegion::from_static(&[0; 4]).is_writable());
}

mod frame;
mod meta;
mod pool;

pub use frame::{
    ExternalBacking, FrameLease, FrameLeaseDescriptor, FramePlaneDescriptor, Plane, PlaneLayout,
    PlaneMut, plane_layout_from_dims, plane_layout_with_stride,
};

#[cfg(unix)]
pub use frame::{FrameBackingExport, FrameExportError, FrameFdPlane};
pub use meta::{
    BackendFrameMeta, FrameMeta, FrameMutability, FrameResidency, ResidencyTransition,
    ResidencyTransitionReason, V4l2FrameMeta,
};
pub use pool::{BufferLease, BufferPool, BufferPoolMetrics, BufferPoolStats};

#[cfg(target_os = "linux")]
pub use pool::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn lazy_pool_starts_empty_and_recycles_on_release() {
        let pool = BufferPool::lazy(16, 2);
        let stats = pool.stats();
        assert_eq!(stats.free, 0);
        assert_eq!(stats.retained, 0);

        let lease = pool.lease();
        assert_eq!(pool.stats().in_use, 1);
        drop(lease);

        let stats = pool.stats();
        assert_eq!(stats.free, 1);
        assert_eq!(stats.retained, 1);
        assert_eq!(stats.retained_bytes, 16);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_pool_reports_active_and_retained_memory() {
        let pool = SharedBufferPool::with_limits(1, 16, 2).expect("shared pool");
        let stats = pool.stats();
        assert_eq!(stats.free, 1);
        assert_eq!(stats.in_use, 0);
        assert_eq!(stats.retained, 1);
        assert_eq!(stats.retained_bytes, 16);

        let lease = pool.lease().expect("lease");
        let stats = pool.stats();
        assert_eq!(stats.free, 0);
        assert_eq!(stats.in_use, 1);
        assert_eq!(stats.retained, 1);
        assert_eq!(stats.in_use_bytes, 16);
        assert_eq!(stats.peak_in_use, 1);
        assert_eq!(stats.hits, 1);
        drop(lease);

        let stats = pool.stats();
        assert_eq!(stats.free, 1);
        assert_eq!(stats.in_use, 0);
        assert_eq!(stats.retained_bytes, 16);
    }

    #[test]
    fn frame_meta_can_carry_v4l2_backend_details() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let meta = FrameMeta::new(fmt, 123).with_backend(BackendFrameMeta::V4l2(V4l2FrameMeta {
            sequence: 7,
            bytes_used: 42,
            field: 1,
            flags: 2,
            zero_copy: true,
        }));

        let v4l2 = meta.v4l2().expect("missing v4l2 metadata");
        assert_eq!(v4l2.sequence, 7);
        assert_eq!(v4l2.bytes_used, 42);
        assert_eq!(v4l2.field, 1);
        assert_eq!(v4l2.flags, 2);
        assert!(v4l2.zero_copy);
    }

    #[test]
    fn frame_meta_can_carry_capture_instant() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let before = std::time::Instant::now();
        let meta = FrameMeta::new(fmt, 123).with_capture_instant(before);

        assert_eq!(meta.capture_instant(), Some(before));
    }

    #[test]
    fn owned_mjpeg_frames_default_to_compressed_packet_residency() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 16);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 8, 8);

        assert_eq!(frame.residency(), FrameResidency::CompressedPacket);
        assert_eq!(frame.mutability(), FrameMutability::Mutable);
    }

    struct TestBacking {
        plane: Vec<u8>,
        drops: Arc<AtomicUsize>,
    }

    impl ExternalBacking for TestBacking {
        fn plane_data(&self, index: usize) -> Option<&[u8]> {
            match index {
                0 => Some(&self.plane),
                _ => None,
            }
        }

        fn backing_bytes(&self) -> Option<usize> {
            Some(self.plane.len())
        }

        fn backing_kind(&self) -> &'static str {
            "test_external"
        }

        fn residency(&self) -> FrameResidency {
            FrameResidency::Dmabuf
        }
    }

    impl Drop for TestBacking {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct MultiPlaneTestBacking {
        planes: Vec<Vec<u8>>,
        drops: Arc<AtomicUsize>,
    }

    impl ExternalBacking for MultiPlaneTestBacking {
        fn plane_data(&self, index: usize) -> Option<&[u8]> {
            self.planes.get(index).map(Vec::as_slice)
        }

        fn backing_bytes(&self) -> Option<usize> {
            Some(self.planes.iter().map(Vec::len).sum())
        }

        fn backing_kind(&self) -> &'static str {
            "test_external_multi"
        }

        fn residency(&self) -> FrameResidency {
            FrameResidency::HostExternal
        }
    }

    impl Drop for MultiPlaneTestBacking {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn external_frame_reports_backing_details_and_borrowed_plane() {
        let drops = Arc::new(AtomicUsize::new(0));
        let res = Resolution::new(2, 1).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = PlaneLayout {
            offset: 0,
            len: 6,
            stride: 6,
        };
        let frame = FrameLease::from_external(
            FrameMeta::new(fmt, 99),
            smallvec::smallvec![layout],
            Arc::new(TestBacking {
                plane: vec![1, 2, 3, 4, 5, 6],
                drops: Arc::clone(&drops),
            }),
        );

        assert!(frame.is_external());
        assert_eq!(frame.external_backing_kind(), Some("test_external"));
        assert_eq!(frame.external_backing_bytes(), Some(6));
        assert_eq!(frame.payload_bytes(), 6);
        assert_eq!(frame.residency(), FrameResidency::Dmabuf);
        assert_eq!(frame.mutability(), FrameMutability::ReadOnly);
        {
            let planes = frame.planes();
            assert_eq!(planes.len(), 1);
            assert_eq!(planes[0].data(), &[1, 2, 3, 4, 5, 6]);
            assert_eq!(planes[0].stride(), 6);
        }

        drop(frame);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn external_frame_into_parts_does_not_try_to_take_owned_buffers() {
        let drops = Arc::new(AtomicUsize::new(0));
        let res = Resolution::new(1, 1).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = PlaneLayout {
            offset: 0,
            len: 3,
            stride: 3,
        };
        let frame = FrameLease::from_external(
            FrameMeta::new(fmt, 77),
            smallvec::smallvec![layout],
            Arc::new(TestBacking {
                plane: vec![9, 8, 7],
                drops: Arc::clone(&drops),
            }),
        );

        let parts = frame.into_parts();
        assert_eq!(parts.meta.timestamp, 77);
        assert_eq!(parts.layouts.len(), 1);
        assert!(parts.buffers.is_empty());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn multi_plane_external_frame_borrows_each_backing_plane() {
        let drops = Arc::new(AtomicUsize::new(0));
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb);
        let frame = FrameLease::from_external(
            FrameMeta::new(fmt, 88),
            smallvec::smallvec![
                PlaneLayout {
                    offset: 0,
                    len: 4,
                    stride: 2,
                },
                PlaneLayout {
                    offset: 0,
                    len: 2,
                    stride: 2,
                },
            ],
            Arc::new(MultiPlaneTestBacking {
                planes: vec![vec![1, 2, 3, 4], vec![5, 6]],
                drops: Arc::clone(&drops),
            }),
        );

        assert!(frame.is_external());
        assert_eq!(frame.external_backing_kind(), Some("test_external_multi"));
        assert_eq!(frame.external_backing_bytes(), Some(6));
        let planes = frame.planes();
        assert_eq!(planes.len(), 2);
        assert_eq!(planes[0].data(), &[1, 2, 3, 4]);
        assert_eq!(planes[1].data(), &[5, 6]);
        drop(planes);
        drop(frame);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn materialize_owned_copies_external_frame_into_mutable_host_buffers() {
        let drops = Arc::new(AtomicUsize::new(0));
        let res = Resolution::new(2, 1).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = PlaneLayout {
            offset: 0,
            len: 6,
            stride: 6,
        };
        let frame = FrameLease::from_external(
            FrameMeta::new(fmt, 12),
            smallvec::smallvec![layout],
            Arc::new(TestBacking {
                plane: vec![10, 20, 30, 40, 50, 60],
                drops: Arc::clone(&drops),
            }),
        );

        let owned = frame.materialize_owned();
        assert!(!owned.is_external());
        assert_eq!(owned.residency(), FrameResidency::HostOwned);
        assert_eq!(owned.mutability(), FrameMutability::Mutable);
        let planes = owned.planes();
        assert_eq!(planes.len(), 1);
        assert_eq!(planes[0].data(), &[10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn frame_descriptor_round_trips_layout_metadata() {
        let res = Resolution::new(4, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709);
        let pool = BufferPool::with_capacity(2, 16);
        let frame = FrameLease::multi_plane(
            FrameMeta::new(fmt, 1234),
            smallvec::smallvec![pool.lease(), pool.lease()],
            smallvec::smallvec![
                PlaneLayout {
                    offset: 0,
                    len: 8,
                    stride: 4,
                },
                PlaneLayout {
                    offset: 8,
                    len: 4,
                    stride: 4,
                },
            ],
        );

        let descriptor = frame.descriptor();
        assert_eq!(descriptor.width, 4);
        assert_eq!(descriptor.height, 2);
        assert_eq!(descriptor.fourcc, FourCc::NV12);
        assert_eq!(descriptor.timestamp, 1234);
        assert_eq!(descriptor.color, ColorSpace::Bt709);
        assert_eq!(descriptor.layouts(), frame.layouts());
        assert_eq!(frame.layout_slice(), frame.layouts().as_slice());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memfd_import_exposes_shared_backing_without_copying() {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let name = CString::new("styx-core-test-frame").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw_fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 6) }, 0);
        let bytes = [1u8, 2, 3, 4, 5, 6];
        let written = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(written, bytes.len() as isize);

        let res = Resolution::new(2, 1).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = PlaneLayout {
            offset: 0,
            len: 6,
            stride: 6,
        };
        let frame =
            FrameLease::from_memfd(FrameMeta::new(fmt, 22), smallvec::smallvec![layout], fd);

        assert!(frame.is_external());
        assert_eq!(frame.external_backing_kind(), Some("memfd"));
        assert_eq!(frame.external_backing_bytes(), Some(6));
        assert_eq!(frame.residency(), FrameResidency::HostExternal);
        let planes = frame.planes();
        assert_eq!(planes[0].data(), &bytes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn export_or_copy_memfd_materializes_owned_frame_for_import() {
        use std::os::fd::AsRawFd;

        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = plane_layout_from_dims(res.width, res.height, 3);
        let pool = BufferPool::with_capacity(1, layout.len);
        let mut lease = pool.lease();
        lease.resize(layout.len);
        for (idx, byte) in lease.as_mut_slice().iter_mut().enumerate() {
            *byte = idx as u8;
        }
        let frame =
            FrameLease::single_plane(FrameMeta::new(fmt, 44), lease, layout.len, layout.stride);

        let (descriptor, export) = frame.export_or_copy_memfd().unwrap();
        let FrameBackingExport::Memfd { fd, len } = export else {
            panic!("owned frame should fall back to memfd");
        };
        assert_eq!(len, layout.len);

        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) }, 0);
        let st = unsafe { st.assume_init() };
        assert_eq!(st.st_size as usize, layout.len);

        let imported = FrameLease::from_memfd_import(descriptor, fd).unwrap();
        let planes = imported.planes();
        assert_eq!(planes.len(), 1);
        assert_eq!(
            planes[0].data(),
            &(0u8..layout.len as u8).collect::<Vec<_>>()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_buffer_pool_frame_exports_without_fallback_copy() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = plane_layout_from_dims(res.width, res.height, 3);
        let pool = SharedBufferPool::with_capacity(1, layout.len).unwrap();
        let mut lease = pool.lease().unwrap();
        lease.try_resize(layout.len).unwrap();
        for (idx, byte) in lease.as_mut_slice().iter_mut().enumerate() {
            *byte = 255u8.saturating_sub(idx as u8);
        }

        let frame = FrameLease::single_plane_shared(
            FrameMeta::new(fmt, 55),
            lease,
            layout.len,
            layout.stride,
        )
        .unwrap();
        assert!(frame.is_external());
        assert_eq!(frame.external_backing_kind(), Some("memfd_pool"));
        assert_eq!(frame.external_backing_bytes(), Some(layout.len));

        let (descriptor, export) = frame.export_or_copy_memfd().unwrap();
        let FrameBackingExport::Memfd { fd, len } = export else {
            panic!("shared buffer frame should export as memfd");
        };
        assert_eq!(len, layout.len);

        let imported = FrameLease::from_memfd_import(descriptor, fd).unwrap();
        let planes = imported.planes();
        let expected: Vec<u8> = (0..layout.len)
            .map(|idx| 255u8.saturating_sub(idx as u8))
            .collect();
        assert_eq!(planes[0].data(), expected.as_slice());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resized_shared_buffer_lease_is_not_recycled_into_fixed_size_pool() {
        let pool = SharedBufferPool::with_limits(1, 8, 1).unwrap();
        assert_eq!(pool.stats().free, 1);

        {
            let mut lease = pool.lease().unwrap();
            assert_eq!(pool.stats().free, 0);
            lease.try_resize(16).unwrap();
            assert_eq!(lease.capacity(), 16);
        }
        assert_eq!(pool.stats().free, 0);

        {
            let lease = pool.lease().unwrap();
            assert_eq!(lease.capacity(), 8);
        }
        assert_eq!(pool.stats().free, 1);
    }
}

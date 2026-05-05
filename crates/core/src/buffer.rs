mod frame;
mod meta;
mod pool;

pub use frame::{
    ExternalBacking, FrameAllocation, FrameLease, FrameLeaseDescriptor, FramePlaneDescriptor,
    FramePlaneShape, FrameValidationError, Plane, PlaneLayout, PlaneMut, VisibleRow, VisibleRowMut,
    VisibleRows, VisibleRowsMut, plane_layout_from_dims, plane_layout_with_stride,
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
        assert!(frame.has_host_readable_bytes());
        assert!(frame.has_host_writable_bytes());
    }

    #[test]
    fn frame_reports_layout_info_and_visible_row_bytes() {
        let res = Resolution::new(4, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let layout = plane_layout_from_dims(res.width, res.height, 3);
        let pool = BufferPool::with_capacity(1, layout.len);
        let frame = FrameLease::single_plane(
            FrameMeta::new(fmt, 7),
            pool.lease(),
            layout.len,
            layout.stride,
        );

        assert_eq!(frame.first_plane_visible_row_bytes(), Some(12));
        assert!(frame.validate_plane_layouts().is_ok());
    }

    #[test]
    fn frame_allocate_host_owned_uses_format_layout() {
        let res = Resolution::new(4, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);

        let frame = FrameLease::allocate_host_owned(fmt, 9).unwrap();

        assert_eq!(frame.residency(), FrameResidency::HostOwned);
        assert_eq!(frame.mutability(), FrameMutability::Mutable);
        assert_eq!(frame.layouts().len(), 1);
        assert_eq!(frame.first_plane_visible_row_bytes(), Some(12));
        let rows = frame.visible_rows(0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.row_bytes(), 12);
        assert_eq!(rows.stride(), 12);
    }

    #[test]
    fn frame_allocation_can_align_strides_and_plane_lengths() {
        let res = Resolution::new(5, 3).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);

        let frame = FrameLease::allocate(FrameAllocation {
            format: fmt,
            timestamp: 9,
            mutability: FrameMutability::Mutable,
            residency: FrameResidency::HostOwned,
            stride_alignment: Some(16),
            plane_alignment: Some(64),
        })
        .unwrap();

        let shape = frame.plane_shape(0).unwrap();
        assert_eq!(shape.row_bytes, 15);
        assert_eq!(shape.stride, 16);
        assert_eq!(shape.len, 64);
        assert!(!frame.is_tightly_packed().unwrap());
        assert_eq!(frame.visible_payload_bytes().unwrap(), 45);
    }

    #[test]
    fn allocate_like_layout_with_timestamp_preserves_layout_only() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 24);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 24, 12);

        let allocated = frame.allocate_like_layout_with_timestamp(99).unwrap();

        assert_eq!(allocated.meta().timestamp, 99);
        assert_eq!(allocated.meta().format, fmt);
        assert_eq!(allocated.layouts(), frame.layouts());
        assert_eq!(allocated.residency(), FrameResidency::HostOwned);
        assert_eq!(allocated.mutability(), FrameMutability::Mutable);
    }

    #[test]
    fn visible_rows_hide_stride_padding() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 24);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 24, 12);

        let rows = frame.visible_rows(0).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.row_bytes(), 9);
        assert_eq!(rows.stride(), 12);
        assert_eq!(rows.row(0).unwrap().data().len(), 9);
        assert_eq!(rows.row(1).unwrap().data().len(), 9);
    }

    #[test]
    fn planes_visible_returns_all_plane_row_views() {
        let res = Resolution::new(5, 3).unwrap();
        let fmt = MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709);
        let frame = FrameLease::allocate_host_owned(fmt, 10).unwrap();

        let planes = frame.planes_visible().unwrap();

        assert_eq!(planes.len(), 2);
        assert_eq!(planes[0].len(), 3);
        assert_eq!(planes[0].row_bytes(), 5);
        assert_eq!(planes[1].len(), 2);
        assert_eq!(planes[1].row_bytes(), 5);
    }

    #[test]
    fn plane_shape_reports_visible_dimensions_and_stride() {
        let res = Resolution::new(5, 3).unwrap();
        let fmt = MediaFormat::new(FourCc::I420, res, ColorSpace::Bt709);
        let frame = FrameLease::allocate_host_owned(fmt, 11).unwrap();

        let luma = frame.plane_shape(0).unwrap();
        let chroma = frame.plane_shape(1).unwrap();

        assert_eq!(luma.width, 5);
        assert_eq!(luma.height, 3);
        assert_eq!(luma.row_bytes, 5);
        assert_eq!(luma.stride, 5);
        assert_eq!(chroma.width, 3);
        assert_eq!(chroma.height, 2);
        assert_eq!(chroma.row_bytes, 3);
        assert_eq!(chroma.stride, 3);
    }

    #[test]
    fn contiguous_visible_plane_fast_path_rejects_padding() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let tight = FrameLease::allocate_host_owned(fmt, 1).unwrap();
        let pool = BufferPool::with_capacity(1, 24);
        let padded = FrameLease::single_plane(FrameMeta::new(fmt, 2), pool.lease(), 24, 12);

        assert_eq!(
            tight
                .try_as_contiguous_visible_plane(0)
                .unwrap()
                .unwrap()
                .len(),
            18
        );
        assert!(padded.try_as_contiguous_visible_plane(0).unwrap().is_none());
    }

    #[test]
    fn tightness_and_visible_payload_ignore_padding() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let tight = FrameLease::allocate_host_owned(fmt, 1).unwrap();
        let pool = BufferPool::with_capacity(1, 24);
        let padded = FrameLease::single_plane(FrameMeta::new(fmt, 2), pool.lease(), 24, 12);

        assert!(tight.is_tightly_packed().unwrap());
        assert!(!padded.is_tightly_packed().unwrap());
        assert_eq!(tight.visible_payload_bytes().unwrap(), 18);
        assert_eq!(padded.visible_payload_bytes().unwrap(), 18);
        assert_eq!(padded.payload_bytes(), 24);
    }

    #[test]
    fn visible_rows_mut_only_exposes_visible_bytes() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 24);
        let mut frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 24, 12);

        frame
            .visible_rows_mut(0)
            .unwrap()
            .for_each_row_mut(|row_index, mut row| {
                row.data().fill((row_index + 1) as u8);
            });

        let planes = frame.planes();
        assert_eq!(&planes[0].data()[0..9], &[1; 9]);
        assert_eq!(&planes[0].data()[9..12], &[0; 3]);
        assert_eq!(&planes[0].data()[12..21], &[2; 9]);
        assert_eq!(&planes[0].data()[21..24], &[0; 3]);
    }

    #[test]
    fn visible_plane_copy_helpers_preserve_padding() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 24);
        let mut frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 24, 12);
        let src: Vec<u8> = (0..18).collect();

        let written = frame.copy_slice_to_visible_plane(0, &src).unwrap();
        let mut copied = vec![0; 18];
        let read = frame.copy_visible_plane_to_slice(0, &mut copied).unwrap();

        assert_eq!(written, 18);
        assert_eq!(read, 18);
        assert_eq!(copied, src);
        let planes = frame.planes();
        assert_eq!(&planes[0].data()[9..12], &[0; 3]);
        assert_eq!(&planes[0].data()[21..24], &[0; 3]);
    }

    #[test]
    fn contiguous_visible_plane_mut_exposes_tight_buffer() {
        let res = Resolution::new(3, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let mut frame = FrameLease::allocate_host_owned(fmt, 1).unwrap();

        let plane = frame
            .try_as_contiguous_visible_plane_mut(0)
            .unwrap()
            .unwrap();
        plane.fill(7);

        assert_eq!(
            frame.try_as_contiguous_visible_plane(0).unwrap().unwrap(),
            &[7; 18]
        );
    }

    #[test]
    fn nv12_allocation_uses_two_plane_420_layout() {
        let res = Resolution::new(5, 3).unwrap();
        let fmt = MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709);

        let frame = FrameLease::allocate_host_owned(fmt, 10).unwrap();

        assert!(frame.validate_plane_layouts().is_ok());
        let layouts = frame.layouts();
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].stride, 5);
        assert_eq!(layouts[0].len, 15);
        assert_eq!(layouts[1].stride, 5);
        assert_eq!(layouts[1].len, 10);
        assert_eq!(frame.visible_rows(1).unwrap().len(), 2);
    }

    #[test]
    fn i420_allocation_uses_three_plane_420_layout() {
        let res = Resolution::new(5, 3).unwrap();
        let fmt = MediaFormat::new(FourCc::I420, res, ColorSpace::Bt709);

        let frame = FrameLease::allocate_host_owned(fmt, 11).unwrap();

        assert!(frame.validate_plane_layouts().is_ok());
        let layouts = frame.layouts();
        assert_eq!(layouts.len(), 3);
        assert_eq!(layouts[0].stride, 5);
        assert_eq!(layouts[0].len, 15);
        assert_eq!(layouts[1].stride, 3);
        assert_eq!(layouts[1].len, 6);
        assert_eq!(layouts[2].stride, 3);
        assert_eq!(layouts[2].len, 6);
        assert_eq!(frame.visible_rows(2).unwrap().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_plane_address_space_rejects_overlapping_layouts() {
        use std::ffi::CString;
        use std::os::fd::{FromRawFd, OwnedFd};

        let name = CString::new("styx-core-overlap-test").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw_fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        assert_eq!(unsafe { libc::ftruncate(raw_fd, 16) }, 0);

        let res = Resolution::new(4, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709);
        let frame = FrameLease::from_memfd(
            FrameMeta::new(fmt, 3),
            smallvec::smallvec![
                PlaneLayout {
                    offset: 0,
                    len: 8,
                    stride: 4,
                },
                PlaneLayout {
                    offset: 4,
                    len: 4,
                    stride: 4,
                },
            ],
            fd,
        );

        let err = frame.validate_plane_layouts().unwrap_err();
        assert!(matches!(
            err,
            FrameValidationError::PlaneRangeOverlap { left: 0, right: 1 }
        ));
    }

    #[test]
    fn frame_reports_capabilities_and_owned_aliasing() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let left = FrameLease::allocate_host_owned(fmt, 1).unwrap();
        let right = FrameLease::allocate_host_owned(fmt, 2).unwrap();

        assert!(left.can_read_planes());
        assert!(left.can_write_planes());
        assert!(!left.can_export());
        assert!(left.can_materialize_without_copy());
        assert!(left.may_alias(&left).unwrap());
        assert!(!left.may_alias(&right).unwrap());
    }

    #[test]
    fn frame_rejects_too_small_stride_for_visible_rows() {
        let res = Resolution::new(4, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let pool = BufferPool::with_capacity(1, 8);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), pool.lease(), 8, 4);

        let err = frame.validate_plane_layouts().unwrap_err();
        assert!(matches!(
            err,
            FrameValidationError::PlaneStrideTooSmall {
                visible_row_bytes: 12,
                ..
            }
        ));
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

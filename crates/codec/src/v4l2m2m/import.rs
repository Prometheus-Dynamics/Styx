//! Frames over dma-bufs handed to a memory-to-memory device by descriptor.

use std::os::fd::AsRawFd;

use lemnos_v4l2::v4l2::MAX_PLANES;
use smallvec::SmallVec;
use styx_core::prelude::*;

use crate::m2m::{DmabufSource, M2mSession};

/// `input`'s memory planes as the device reads them, written to `out` (their count returned),
/// if it lies in dma-bufs at the session's source layout: the colour planes the device keeps
/// in one memory plane in one dma-buf, at its offsets and strides. `None` for any other frame.
pub(super) fn dmabuf_planes<'a>(
    session: &M2mSession<FrameLease>,
    input: &'a FrameLease,
    out: &mut [Option<DmabufSource<'a>>; MAX_PLANES],
) -> Option<usize> {
    use lemnos_linux_sys::dma_buf::identity;
    let dmabuf_id = |fd| identity(fd).ok();
    let planes = input.planes_visible().ok()?;
    let shape: SmallVec<[(usize, usize); 3]> =
        planes.iter().map(|p| (p.row_bytes(), p.len())).collect();
    let spans = session.source_spans(&shape);
    *out = [None; MAX_PLANES];
    let mut count = 0;
    for (i, (span, plane)) in spans.iter().zip(planes.iter()).enumerate() {
        let at = input.dmabuf_plane(i)?;
        let base = at.offset.checked_sub(span.offset)?;
        if plane.stride() != span.stride && plane.len() > 1 {
            return None;
        }
        let (row_bytes, rows) = shape[i];
        let end = span.offset + span.stride * rows.saturating_sub(1) + row_bytes;
        let slot = out.get_mut(span.buffer)?;
        match slot {
            Some(have) => {
                let same = have.fd.as_raw_fd() == at.fd.as_raw_fd()
                    || dmabuf_id(have.fd).is_some_and(|id| dmabuf_id(at.fd) == Some(id));
                if !same || have.offset as usize != base {
                    return None;
                }
                have.bytes = have.bytes.max(u32::try_from(end).ok()?);
            }
            None => {
                *slot = Some(DmabufSource {
                    fd: at.fd,
                    offset: u32::try_from(base).ok()?,
                    bytes: u32::try_from(end).ok()?,
                });
            }
        }
        count = count.max(span.buffer + 1);
    }
    out[..count].iter().all(Option::is_some).then_some(count)
}

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests {
    use smallvec::smallvec;
    use styx_native::dma::{Access, DmaHeap};

    use super::super::{V4l2M2mConverter, V4l2M2mOptions};
    use crate::Codec;
    use styx_core::prelude::*;

    /// A `width` x `height` RGB24 frame in a system-heap dma-buf, rows `stride` bytes apart,
    /// numbered by `seed`.
    fn rgb_dmabuf(
        heap: &DmaHeap,
        (width, height): (u32, u32),
        stride: usize,
        seed: u8,
    ) -> FrameLease {
        let len = stride * height as usize;
        let buf = heap.allocate(len).unwrap();
        let mut map = buf.map().unwrap();
        buf.begin_cpu_access(Access::Write).unwrap();
        for (i, b) in map.as_mut_slice()[..len].iter_mut().enumerate() {
            *b = ((i % stride) as u8).wrapping_mul(3).wrapping_add(seed);
        }
        buf.end_cpu_access(Access::Write).unwrap();
        drop(map);
        let format = MediaFormat::new(
            FourCc::RGB3,
            Resolution::new(width, height).unwrap(),
            ColorSpace::Srgb,
        );
        let layouts = smallvec![PlaneLayout {
            offset: 0,
            len,
            stride,
        }];
        let planes = vec![FrameFdPlane {
            fd: buf.into_fd(),
            offset: 0,
            len,
        }];
        FrameLease::from_dmabuf(FrameMeta::new(format, 3), layouts, planes).unwrap()
    }

    fn same_rows(a: &FrameLease, b: &FrameLease) -> bool {
        let (a, b) = (a.planes_visible().unwrap(), b.planes_visible().unwrap());
        (0..a[0].len()).all(|r| a[0].row(r) == b[0].row(r))
    }

    /// vim2m imports a dma-buf frame at its own layout and reads it in place; a frame at a
    /// stride the device does not take is copied instead, and so are the frames after it.
    /// Skipped without vim2m or the system dma-heap.
    #[test]
    fn dmabuf_frames_are_read_in_place_or_copied() {
        let rgb = FourCc::RGB3;
        let Ok(heap) = DmaHeap::open("system") else {
            eprintln!("skipped: no system dma-heap");
            return;
        };
        if !V4l2M2mConverter::available(rgb, rgb) {
            eprintln!("skipped: no vim2m (modprobe vim2m)");
            return;
        }
        let codec = V4l2M2mConverter::new(rgb, rgb, None, V4l2M2mOptions::default());
        let size = (64, 48);
        for seed in 0..3 {
            let input = rgb_dmabuf(&heap, size, 64 * 3, seed);
            let out = codec.process_ref(&input).unwrap();
            assert!(same_rows(&input, &out), "frame {seed}");
            let guard = codec.stream.lock();
            assert!(guard.as_ref().unwrap().session.imports());
        }
        // Rows padded past what vim2m lays out: refused, copied from then on.
        let padded = rgb_dmabuf(&heap, size, 64 * 3 + 64, 9);
        let out = codec.process_ref(&padded).unwrap();
        assert!(same_rows(&padded, &out));
        assert!(!codec.stream.lock().as_ref().unwrap().session.imports());
        let input = rgb_dmabuf(&heap, size, 64 * 3, 4);
        assert!(same_rows(&input, &codec.process_ref(&input).unwrap()));
        assert!(!codec.stream.lock().as_ref().unwrap().session.imports());
    }
}

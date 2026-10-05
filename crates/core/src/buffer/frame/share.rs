//! One frame shared by several consumers without copying its pixels.

use super::*;

/// Pooled plane buffers behind shared ownership: they return to their pool when the last view
/// of the frame is dropped.
struct SharedPlanes {
    buffers: SmallVec<[BufferLease; 3]>,
    residency: FrameResidency,
}

impl ExternalBacking for SharedPlanes {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.buffers.get(index).map(BufferLease::as_slice)
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.buffers.iter().map(|b| b.as_slice().len()).sum())
    }

    fn backing_kind(&self) -> &'static str {
        "shared_pool"
    }

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        for buffer in &self.buffers {
            let bytes = buffer.as_slice();
            span(bytes.as_ptr() as usize, bytes.len());
        }
        true
    }

    fn residency(&self) -> FrameResidency {
        self.residency
    }

    fn cpu_access(&self) -> super::super::cpu_access::CpuAccess {
        // Pooled host buffers, whatever the frame's residency says.
        super::super::cpu_access::CpuAccess::Cached
    }
}

impl FrameLease {
    /// This frame with its pixels (and its companions') under shared ownership, so
    /// [`FrameLease::share`] can hand out more views of it. Frames in driver or device buffers
    /// are already shared; pooled frames move their buffers once, without copying. The result is
    /// read-only.
    pub fn into_shareable(mut self) -> FrameLease {
        let companions = self.take_companions();
        if self.external.is_none() {
            let residency = self.residency();
            let buffers = core::mem::take(&mut self.buffers);
            self.external = Some(shared_backing(SharedPlanes { buffers, residency }));
            self.meta.mutability = FrameMutability::ReadOnly;
        }
        for (kind, companion) in companions {
            let companion = companion.into_shareable();
            let companions = self.companions.get_or_insert_with(Default::default);
            companions.push((kind, companion));
        }
        self
    }

    /// Another view of the same pixels, metadata and companions, without copying. `None` when
    /// the frame owns its buffers; [`FrameLease::into_shareable`] makes any frame shareable.
    pub fn share(&self) -> Option<FrameLease> {
        // The views read what this frame wrote: end its CPU writes first.
        self.finish_cpu_write();
        let mut out = FrameLease {
            meta: self.meta.clone(),
            buffers: SmallVec::new(),
            layouts: self.layouts.clone(),
            external: Some(self.external.clone()?),
            companions: None,
        };
        // Views read; only a sole owner writes.
        out.meta.mutability = FrameMutability::ReadOnly;
        for (kind, companion) in self.companions() {
            let companions = out.companions.get_or_insert_with(Default::default);
            companions.push((kind, companion.share()?));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, Resolution};

    #[test]
    fn shared_views_see_the_same_pixels_and_release_the_buffer_last() {
        let pool = BufferPool::with_limits(0, 16, 4);
        let mut buf = pool.lease();
        buf.resize(64);
        buf.as_mut_slice()
            .iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = i as u8);
        let format = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(8, 8).unwrap(),
            ColorSpace::Unknown,
        );
        let frame = FrameLease::single_plane(FrameMeta::new(format, 7), buf, 64, 8)
            .with_box_pyramid(1, 1)
            .unwrap();
        assert!(
            frame.share().is_none(),
            "owned frames need into_shareable first"
        );

        let shared = frame.into_shareable();
        let (a, b) = (shared.share().unwrap(), shared.share().unwrap());
        drop(shared);
        assert_eq!(a.planes()[0].data(), b.planes()[0].data());
        assert_eq!(a.planes()[0].data()[9], 9);
        assert_eq!(a.meta().timestamp, 7);
        assert!(a.pyramid_level(1).is_some() && b.pyramid_level(1).is_some());
        assert_eq!(pool.stats().free, 0, "still in use by the views");
        drop((a, b));
        assert_eq!(pool.stats().free, 1, "returned with the last view");
    }
}

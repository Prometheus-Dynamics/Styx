//! Bracketed CPU reads of a frame's planes: begun and ended in pairs, possibly overlapping,
//! for consumers that say when they are done (Daedalus's `daedalus:frame` access).

use super::FrameLease;

impl FrameLease {
    /// Begins a bracketed CPU read of plane `index`: the plane's bytes, or `None` when the CPU
    /// cannot read them here (nothing to end then). Every `Some` is followed by exactly one
    /// [`FrameLease::end_cpu_read`] for the plane; the bytes stay valid and synced for the CPU
    /// until then. Frames in their own memory need nothing more; a backing over device memory
    /// maps on first use and counts the reads ([`ExternalBacking::begin_cpu_read`]): its cache
    /// maintenance starts with the first open read and ends with the last, where
    /// [`FrameLease::plane_at`]'s first read keeps it open until the backing drops.
    ///
    /// [`ExternalBacking::begin_cpu_read`]: super::ExternalBacking::begin_cpu_read
    pub fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        #[cfg(feature = "path-metrics")]
        crate::metrics::path_counters().cpu_reads.incr();
        if !self.cpu_access().readable() {
            return None;
        }
        let layout = self.layouts.get(index)?;
        let span = layout.offset..layout.offset.checked_add(layout.len)?;
        let Some(backing) = &self.external else {
            return self
                .buffers
                .get(index)?
                .as_slice()
                .get(span)
                .filter(|b| !b.is_empty());
        };
        let bytes = backing.begin_cpu_read(index)?;
        let plane = bytes.get(span).filter(|b| !b.is_empty());
        if plane.is_none() {
            backing.end_cpu_read(index);
        }
        plane
    }

    /// Ends a read begun by a `Some` from [`FrameLease::begin_cpu_read`].
    pub fn end_cpu_read(&self, index: usize) {
        if let Some(backing) = &self.external {
            backing.end_cpu_read(index);
        }
    }
}

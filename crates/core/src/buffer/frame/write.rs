//! Writing a frame's planes in place: its own buffers, or a writable external backing (a
//! [`MemoryRegion`](super::MemoryRegion) over caller-provided memory) while the frame is its one
//! owner.

use super::*;

/// Whether `backing` has no other owner: no shared views, no handles, no weak references.
pub(super) fn sole_owner(backing: &Arc<dyn ExternalBacking>) -> bool {
    Arc::strong_count(backing) == 1 && Arc::weak_count(backing) == 0
}

impl FrameLease {
    /// One plane's bytes for writing (its layout's range), or `None` when the CPU cannot write
    /// them. Frames in their own buffers grow the buffer to the layout if needed; frames over
    /// external memory need a writable backing ([`MemoryRegion::from_raw_mut`](super::MemoryRegion::from_raw_mut))
    /// this frame is the one owner of. The first write to a region opens its CPU write window
    /// ([`RegionHooks::begin_cpu_write`](super::RegionHooks::begin_cpu_write)).
    pub fn plane_data_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        let layout = *self.layouts.get(index)?;
        let end = layout.offset.checked_add(layout.len)?;
        match self.external.as_mut() {
            None => {
                let buffer = self.buffers.get_mut(index)?;
                if buffer.len() < end {
                    buffer.resize(end);
                }
                buffer.as_mut_slice().get_mut(layout.offset..end)
            }
            Some(backing) => {
                if !backing.cpu_writable() {
                    return None;
                }
                Arc::get_mut(backing)?
                    .plane_data_mut(index)?
                    .get_mut(layout.offset..end)
            }
        }
    }

    /// Ends the CPU writes to this frame's external memory now (cache maintenance so a device
    /// reads them: [`RegionHooks::end_cpu_write`](super::RegionHooks::end_cpu_write)), rather
    /// than when the frame is shared, its backing handed out, or dropped. Nothing for frames in
    /// their own buffers or with no writes open; writing again opens a new window.
    pub fn finish_cpu_write(&self) {
        if let Some(backing) = &self.external {
            backing.finish_cpu_write();
        }
    }

    /// [`FrameLease::planes_mut`] over external memory: every plane at once, cut from the
    /// backing's buffers, or empty slices when the frame cannot write them (planes whose ranges
    /// fall outside their buffer or overlap an earlier plane's are empty too).
    pub(super) fn external_planes_mut(&mut self) -> SmallVec<[PlaneMut<'_>; 3]> {
        let layouts = &self.layouts;
        let Some(backing) = self
            .external
            .as_mut()
            .filter(|backing| backing.cpu_writable())
            .and_then(Arc::get_mut)
        else {
            return empty_planes(layouts);
        };

        // One call per plane, kept as raw parts. Planes sharing a buffer (a region holds them
        // all) are cut from the newest pointer to it: each call reborrows the whole buffer,
        // which retires the pointers earlier calls gave for it.
        let mut buffers: SmallVec<[(*mut u8, usize); 3]> = SmallVec::new();
        for index in 0..layouts.len() {
            let (ptr, len) = backing
                .plane_data_mut(index)
                .map_or((core::ptr::null_mut(), 0), |s| (s.as_mut_ptr(), s.len()));
            // The same address, but the new pointer's provenance: the one still valid.
            for buffer in buffers.iter_mut().filter(|b| b.0 == ptr && b.1 == len) {
                buffer.0 = ptr;
            }
            buffers.push((ptr, len));
        }
        // Different buffers must not overlap (the trait's contract); if a backing breaks it,
        // nothing is written.
        for (i, &(a, a_len)) in buffers.iter().enumerate() {
            for &(b, b_len) in &buffers[..i] {
                let same = a == b && a_len == b_len;
                let (a0, b0) = (a as usize, b as usize);
                let overlap = a0 < b0.saturating_add(b_len) && b0 < a0.saturating_add(a_len);
                if !same && overlap && a_len > 0 && b_len > 0 {
                    return empty_planes(layouts);
                }
            }
        }

        let mut granted: SmallVec<[(usize, usize); 3]> = SmallVec::new();
        let mut planes = SmallVec::with_capacity(layouts.len());
        for (index, layout) in layouts.iter().enumerate() {
            let (ptr, len) = buffers[index];
            let in_bounds = layout
                .offset
                .checked_add(layout.len)
                .is_some_and(|end| end <= len);
            let start = (ptr as usize).wrapping_add(layout.offset);
            let end = start.wrapping_add(layout.len);
            let overlaps = granted.iter().any(|&(s, e)| start < e && s < end);
            let data: &mut [u8] = if ptr.is_null() || !in_bounds || overlaps || layout.len == 0 {
                &mut []
            } else {
                granted.push((start, end));
                // SAFETY: `ptr` is the newest pointer the backing gave for this buffer
                // (`buffers` holds it for every plane in it), valid for `len` bytes, and no later
                // call reborrowed this buffer (other buffers do not overlap it). The range is
                // inside it and does not overlap any other plane's, so the slices are disjoint;
                // they borrow `self` mutably, so the backing stays uniquely ours meanwhile.
                unsafe { core::slice::from_raw_parts_mut(ptr.add(layout.offset), layout.len) }
            };
            planes.push(PlaneMut {
                data,
                stride: layout.stride,
            });
        }
        planes
    }
}

fn empty_planes<'a>(layouts: &[PlaneLayout]) -> SmallVec<[PlaneMut<'a>; 3]> {
    layouts
        .iter()
        .map(|layout| PlaneMut {
            data: &mut [],
            stride: layout.stride,
        })
        .collect()
}

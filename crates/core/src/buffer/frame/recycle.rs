//! Frame backings without an allocation per frame: a [`BackingPool`] hands a frame's backing
//! record back when its last lease drops, so the next frame reuses the allocation. The wrapped
//! backing's own `Drop` (giving a buffer back to its driver) runs at the same moment as before.

use alloc::vec::Vec;

use super::{CpuAccess, ExternalBacking, FrameResidency};
use crate::sync::{Arc, Mutex};

type Free<B> = Arc<Mutex<Vec<Arc<Pooled<B>>>>>;

struct Pooled<B: ExternalBacking + 'static> {
    /// `None` only on the pool's free list, where nothing reads it.
    inner: Option<B>,
    free: Free<B>,
}

impl<B: ExternalBacking + 'static> Pooled<B> {
    fn get(&self) -> &B {
        self.inner.as_ref().expect("a backing in use")
    }
}

/// Reusable allocations for one kind of [`ExternalBacking`].
pub struct BackingPool<B: ExternalBacking + 'static> {
    free: Free<B>,
    keep: usize,
}

impl<B: ExternalBacking + 'static> Clone for BackingPool<B> {
    fn clone(&self) -> Self {
        Self {
            free: Arc::clone(&self.free),
            keep: self.keep,
        }
    }
}

impl<B: ExternalBacking + 'static> BackingPool<B> {
    /// A pool that keeps up to `keep` allocations (the frames in flight at most; more are freed).
    pub fn new(keep: usize) -> Self {
        Self {
            free: Arc::new(Mutex::new(Vec::with_capacity(keep))),
            keep,
        }
    }

    /// `backing` as a frame's backing, in a reused allocation when one is free.
    pub fn wrap(&self, backing: B) -> Arc<dyn ExternalBacking> {
        let reused = self.free.lock().pop();
        if let Some(mut slot) = reused
            && let Some(pooled) = Arc::get_mut(&mut slot)
        {
            pooled.inner = Some(backing);
            return slot;
        }
        Arc::new(Pooled {
            inner: Some(backing),
            free: Arc::clone(&self.free),
        })
    }
}

impl<B: ExternalBacking + 'static> ExternalBacking for Pooled<B> {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.get().plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.get().backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        self.get().backing_kind()
    }

    fn can_export(&self) -> bool {
        self.get().can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.get().residency()
    }

    fn progress(&self) -> Option<crate::buffer::FrameProgressRef<'_>> {
        self.get().progress()
    }

    #[cfg(feature = "std")]
    fn progress_handle(&self) -> Option<crate::buffer::FrameProgressHandle> {
        self.get().progress_handle()
    }

    fn cpu_access(&self) -> CpuAccess {
        self.get().cpu_access()
    }

    #[cfg(all(feature = "std", unix))]
    fn export_backing(&self) -> Result<Option<super::FrameBackingExport>, super::FrameExportError> {
        self.get().export_backing()
    }

    #[cfg(all(feature = "std", unix))]
    fn export_into(
        &self,
        out: &mut Vec<super::FrameFdPlane>,
    ) -> Result<Option<super::ExportedKind>, super::FrameExportError> {
        self.get().export_into(out)
    }

    fn cpu_writable(&self) -> bool {
        self.get().cpu_writable()
    }

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        self.get().host_spans(span)
    }

    fn plane_data_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        self.inner.as_mut()?.plane_data_mut(index)
    }

    fn finish_cpu_write(&self) {
        self.get().finish_cpu_write()
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        self.get().begin_cpu_read(index)
    }

    fn end_cpu_read(&self, index: usize) {
        self.get().end_cpu_read(index)
    }

    #[cfg(all(feature = "std", unix))]
    fn dmabuf_plane(&self, index: usize) -> Option<super::DmabufPlane<'_>> {
        self.get().dmabuf_plane(index)
    }

    fn recycle(mut self: Arc<Self>) {
        let free = Arc::clone(&self.free);
        if let Some(pooled) = Arc::get_mut(&mut self) {
            // The wrapped backing's drop, as if the allocation had gone with it.
            pooled.inner = None;
            let mut free = free.lock();
            if free.len() < free.capacity() {
                free.push(self);
            }
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct Counted(Arc<AtomicU32>);

    impl ExternalBacking for Counted {
        fn plane_data(&self, _: usize) -> Option<&[u8]> {
            None
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn address(backing: &Arc<dyn ExternalBacking>) -> *const () {
        Arc::as_ptr(backing).cast()
    }

    #[test]
    fn the_last_reference_recycles_and_the_backing_drops_then() {
        let drops = Arc::new(AtomicU32::new(0));
        let pool = BackingPool::new(2);
        let first = pool.wrap(Counted(Arc::clone(&drops)));
        let at = address(&first);
        let held = Arc::clone(&first);
        // Not the last: nothing happens.
        drop(first);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        held.recycle();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let second = pool.wrap(Counted(Arc::clone(&drops)));
        assert_eq!(address(&second), at, "the allocation is reused");
        // An ordinary drop (no lease in the way) still drops the backing.
        drop(second);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_pool_keeps_no_more_than_it_was_asked_to() {
        let drops = Arc::new(AtomicU32::new(0));
        let pool = BackingPool::new(1);
        let (a, b) = (
            pool.wrap(Counted(Arc::clone(&drops))),
            pool.wrap(Counted(Arc::clone(&drops))),
        );
        a.recycle();
        b.recycle();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(pool.free.lock().len(), 1);
    }
}

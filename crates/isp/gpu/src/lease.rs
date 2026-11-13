//! The GPU lease hook: the product asks for the GPU before the ISP's work and releases it
//! after, so GPU work is shared the way the product decides (Lemnos's `gpu-lease/1` is one
//! such owner). With no hook set the ISP asks nothing and costs nothing.

use std::sync::Arc;

use crate::error::GpuError;

/// Asked by a [`GpuIsp`](crate::GpuIsp) before each frame's GPU work and released after it.
///
/// `acquire` returning an error refuses the frame: the ISP returns [`GpuError::LeaseDenied`]
/// and makes no GPU work for it. `release` is called once for every successful `acquire`,
/// after the frame's work has finished (its fence was waited on), also when the work failed.
pub trait GpuLease: Send + Sync {
    /// Asks for the GPU. `Err` carries the reason it was refused.
    fn acquire(&self) -> Result<(), String>;
    /// Gives the GPU back.
    fn release(&self);
}

/// A held lease, released when dropped.
pub(crate) struct LeaseHeld {
    lease: Arc<dyn GpuLease>,
}

impl LeaseHeld {
    pub(crate) fn take(lease: Arc<dyn GpuLease>) -> Result<Self, GpuError> {
        lease.acquire().map_err(GpuError::LeaseDenied)?;
        Ok(Self { lease })
    }
}

impl Drop for LeaseHeld {
    fn drop(&mut self) {
        self.lease.release();
    }
}

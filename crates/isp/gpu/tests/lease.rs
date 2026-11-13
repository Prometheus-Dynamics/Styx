//! The GPU lease hook: asked before each frame's GPU work and released after it; refused frames
//! make no GPU work. Skips with a printed reason when the host has no Vulkan device.
#![cfg(feature = "vulkan")]

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use styx_format::FourCc;
use styx_gpu::{DeviceSelect, GpuContext, GpuError, GpuIspStage, GpuLease};
use styx_soft::params::{Demosaic, YuvMatrix};

#[derive(Default)]
struct Counting {
    acquired: AtomicUsize,
    released: AtomicUsize,
    refuse: bool,
}

impl GpuLease for Counting {
    fn acquire(&self) -> Result<(), String> {
        if self.refuse {
            return Err("the test refuses".into());
        }
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn release(&self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

fn device() -> Option<GpuContext> {
    if styx_gpu::devices().is_empty() {
        eprintln!("skipped: no suitable Vulkan device on this host");
        return None;
    }
    GpuContext::open(DeviceSelect::Auto).ok().or_else(|| {
        eprintln!("skipped: the Vulkan device did not open");
        None
    })
}

#[test]
fn lease_is_asked_before_and_released_after_each_frame() {
    let Some(ctx) = device() else { return };
    let settings = settings(true, true);
    let options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, true);
    let mut stage = GpuIspStage::with_context(Some(&ctx), RAW, W, H, &settings, options).unwrap();
    let lease = Arc::new(Counting::default());
    stage.set_gpu_lease(Some(lease.clone()));

    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    let out_pool = picture_pool(FourCc::RGB3, W, H, 1);
    let mut out = picture(&out_pool, FourCc::RGB3, W, H, 2);
    let mut stats = styx_aaa::Statistics::default();
    for _ in 0..3 {
        stage.process(&raw, &mut out, Some(&mut stats)).unwrap();
    }
    assert_eq!(lease.acquired.load(Ordering::SeqCst), 3);
    assert_eq!(lease.released.load(Ordering::SeqCst), 3);
}

#[test]
fn refused_lease_makes_no_frame() {
    let Some(ctx) = device() else { return };
    let settings = settings(false, false);
    let options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, false);
    let mut stage = GpuIspStage::with_context(Some(&ctx), RAW, W, H, &settings, options).unwrap();
    stage.set_gpu_lease(Some(Arc::new(Counting {
        refuse: true,
        ..Counting::default()
    })));

    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    let out_pool = picture_pool(FourCc::RGB3, W, H, 1);
    let mut out = picture(&out_pool, FourCc::RGB3, W, H, 2);
    let err = stage.process(&raw, &mut out, None).unwrap_err();
    assert!(
        matches!(err, styx_gpu::StageError::Gpu(GpuError::LeaseDenied(_))),
        "{err:?}"
    );
    let read = out.read().unwrap();
    assert!(
        read.plane(0)
            .unwrap()
            .row_bytes(0, (W * 3) as usize)
            .unwrap()
            .iter()
            .all(|&b| b == 0)
    );
}

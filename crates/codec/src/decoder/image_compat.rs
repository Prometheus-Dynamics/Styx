#![cfg(feature = "image")]

use crate::decoder::raw::yuv_to_rgb;
use crate::{Codec, CodecError};
use image::{DynamicImage, GenericImageView};
use rayon::prelude::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use styx_core::prelude::{
    BufferPool, BufferPoolStats, ColorSpace, FourCc, FrameLease, FrameMeta, MediaFormat, Resolution,
};
use yuvutils_rs::{
    YuvBiPlanarImage, YuvConversionMode, YuvPackedImage, YuvPlanarImage, YuvRange,
    YuvStandardMatrix,
};

/// Trait to retrieve a `DynamicImage` from any decoder.
pub trait ImageDecode {
    fn decode_image(&self, frame: FrameLease) -> Result<DynamicImage, CodecError>;
}

#[inline(always)]
fn preferred_yuv_conversion_mode() -> YuvConversionMode {
    #[cfg(target_arch = "aarch64")]
    {
        YuvConversionMode::Fast
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        YuvConversionMode::Balanced
    }
}

pub(crate) fn process_to_dynamic<D: Codec>(
    decoder: &D,
    frame: FrameLease,
) -> Result<DynamicImage, CodecError> {
    match frame_lease_to_dynamic_image(frame) {
        Ok(img) => Ok(img),
        Err(frame) => {
            if let Some(img) = frame_to_dynamic_image(&frame) {
                return Ok(img);
            }
            let decoded = decoder.process(frame)?;
            match frame_lease_to_dynamic_image(decoded) {
                Ok(img) => Ok(img),
                Err(decoded) => frame_to_dynamic_image(&decoded)
                    .ok_or_else(|| CodecError::Codec("unable to convert to DynamicImage".into())),
            }
        }
    }
}

thread_local! {
    static PACKED_FRAME_POOLS: RefCell<Vec<(usize, BufferPool)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub struct PackedFramePoolStats {
    pub min_len: usize,
    pub stats: BufferPoolStats,
}

const PACKED_FRAME_POOL_SLOTS: usize = 4;

fn packed_frame_pool(len: usize) -> BufferPool {
    PACKED_FRAME_POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        if let Some(pos) = pools.iter().position(|(k, _)| *k == len) {
            let (k, pool) = pools.remove(pos);
            pools.insert(0, (k, pool.clone()));
            return pool;
        }

        let pool = BufferPool::with_limits(2, len, 2);
        pools.insert(0, (len, pool.clone()));
        if pools.len() > PACKED_FRAME_POOL_SLOTS {
            pools.truncate(PACKED_FRAME_POOL_SLOTS);
        }
        pool
    })
}

pub fn clear_packed_frame_pools() {
    PACKED_FRAME_POOLS.with(|pools| pools.borrow_mut().clear());
}

pub fn clear_packed_frame_pools_all_threads() {
    clear_packed_frame_pools();
    rayon::broadcast(|_| clear_packed_frame_pools());
}

pub fn packed_frame_pool_stats() -> Vec<PackedFramePoolStats> {
    PACKED_FRAME_POOLS.with(|pools| {
        pools
            .borrow()
            .iter()
            .map(|(len, pool)| PackedFramePoolStats {
                min_len: *len,
                stats: pool.stats(),
            })
            .collect()
    })
}

#[derive(Clone, Debug, Default)]
pub struct StagingCopyStats {
    pub copies: u64,
    pub bytes: u64,
    pub peak_copy_bytes: u64,
}

static STAGING_COPY_COUNT: AtomicU64 = AtomicU64::new(0);
static STAGING_COPY_BYTES: AtomicU64 = AtomicU64::new(0);
static STAGING_COPY_PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

fn record_staging_copy(bytes: usize) {
    let bytes = bytes as u64;
    STAGING_COPY_COUNT.fetch_add(1, Ordering::Relaxed);
    STAGING_COPY_BYTES.fetch_add(bytes, Ordering::Relaxed);
    STAGING_COPY_PEAK_BYTES.fetch_max(bytes, Ordering::Relaxed);
}

pub fn staging_copy_stats() -> StagingCopyStats {
    StagingCopyStats {
        copies: STAGING_COPY_COUNT.load(Ordering::Relaxed),
        bytes: STAGING_COPY_BYTES.load(Ordering::Relaxed),
        peak_copy_bytes: STAGING_COPY_PEAK_BYTES.load(Ordering::Relaxed),
    }
}

include!("image_compat_impl.incl.rs");

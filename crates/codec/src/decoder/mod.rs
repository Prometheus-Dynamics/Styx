//! Decoder namespace with per-format modules.

#[cfg(feature = "codec-ffmpeg")]
pub mod ffmpeg;
#[cfg(feature = "image")]
mod image_compat;
#[cfg(feature = "codec-jpeg-decoder")]
pub mod mjpeg;
pub mod raw;

#[cfg(feature = "image")]
pub(crate) use image_compat::process_to_dynamic;
#[cfg(feature = "image")]
pub use image_compat::{
    ImageDecode, PackedFramePoolStats, StagingCopyStats, clear_packed_frame_pools,
    clear_packed_frame_pools_all_threads, dynamic_image_ref_to_rg24_frame,
    dynamic_image_to_rg24_frame, frame_lease_to_dynamic_image, frame_to_dynamic_image,
    packed_frame_pool_stats, staging_copy_stats,
};

#![cfg(any(feature = "netcam-video", feature = "file-backend-video"))]

use ffmpeg_next::frame::Video as FfFrame;
use styx_core::prelude::*;

/// Copy an FFmpeg RGBA frame into a pooled `FrameLease`.
pub(crate) fn blit_rgba_frame(
    rgb: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let stride = rgb.stride(0) as usize;
    let data = rgb.data(0);
    let mut lease = pool.lease();
    lease.resize(layout.len);
    for (y, chunk) in lease.as_mut_slice().chunks_mut(layout.stride).enumerate() {
        let start = y * stride;
        let end = start + layout.stride.min(data.len().saturating_sub(start));
        if end > start && end <= data.len() {
            chunk[..end - start].copy_from_slice(&data[start..end]);
        }
    }
    FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::new(*b"RGBA"), res, ColorSpace::Srgb),
            timestamp,
        ),
        lease,
        layout.len,
        layout.stride,
    )
}

/// Convert an FFmpeg RGBA frame into packed `RG24` output.
pub(crate) fn blit_rgb24_frame(
    rgba: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let stride = rgba.stride(0) as usize;
    let data = rgba.data(0);
    let width = res.width.get() as usize;
    let mut lease = pool.lease();
    lease.resize(layout.len);
    let dst = lease.as_mut_slice();
    for y in 0..res.height.get() as usize {
        let src_row = y.saturating_mul(stride);
        let dst_row = y.saturating_mul(layout.stride);
        for x in 0..width {
            let src_idx = src_row.saturating_add(x * 4);
            let dst_idx = dst_row.saturating_add(x * 3);
            if src_idx + 2 >= data.len() || dst_idx + 2 >= dst.len() {
                break;
            }
            dst[dst_idx] = data[src_idx];
            dst[dst_idx + 1] = data[src_idx + 1];
            dst[dst_idx + 2] = data[src_idx + 2];
        }
    }
    FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb),
            timestamp,
        ),
        lease,
        layout.len,
        layout.stride,
    )
}

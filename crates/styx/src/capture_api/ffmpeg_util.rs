#![cfg(any(feature = "netcam-video", feature = "file-backend-video"))]

use ffmpeg_next::frame::Video as FfFrame;
use ffmpeg_next::{
    codec::{self, Id},
    decoder,
};
use styx_core::prelude::*;

/// Open a video decoder with a best-effort hardware preference for H.264/H.265.
///
/// We first try explicit hardware decoder names that are common on Linux SBCs and
/// fall back to FFmpeg's default decoder resolution when unavailable.
pub(crate) fn open_preferred_video_decoder(
    parameters: &codec::Parameters,
    prefer_hardware: bool,
) -> Result<decoder::Video, ffmpeg_next::Error> {
    let candidates: &[&str] = if prefer_hardware {
        match parameters.id() {
            Id::H264 => &["h264_v4l2request", "h264_v4l2m2m"],
            Id::HEVC => &["hevc_v4l2request", "hevc_v4l2m2m"],
            _ => &[],
        }
    } else {
        &[]
    };

    for name in candidates {
        let Some(codec_impl) = codec::decoder::find_by_name(name) else {
            continue;
        };
        let mut context = codec::Context::new_with_codec(codec_impl);
        if context.set_parameters(parameters.clone()).is_err() {
            continue;
        }
        if let Ok(video) = context.decoder().video() {
            return Ok(video);
        }
    }

    codec::Context::from_parameters(parameters.clone())?
        .decoder()
        .video()
}

/// Copy an FFmpeg RGBA frame into a pooled `FrameLease`.
// Used by non-Linux or non-shared-memory video feature combinations.
#[allow(dead_code)]
pub(crate) fn blit_rgba_frame(
    rgb: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let stride = rgb.stride(0);
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
            MediaFormat::new(FourCc::RGBA, res, ColorSpace::Srgb),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::Capture,
            copied: true,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

/// Copy an FFmpeg RGBA frame into exportable shared memory.
#[cfg(target_os = "linux")]
// Used by Linux video paths when the selected feature combination can export shared frames.
#[allow(dead_code)]
pub(crate) fn blit_shared_rgba_frame(
    rgb: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &SharedBufferPool,
    timestamp: u64,
) -> Result<FrameLease, styx_core::buffer::FrameExportError> {
    let stride = rgb.stride(0);
    let data = rgb.data(0);
    let mut lease = pool.lease()?;
    lease.try_resize(layout.len)?;
    for (y, chunk) in lease.as_mut_slice().chunks_mut(layout.stride).enumerate() {
        let start = y * stride;
        let end = start + layout.stride.min(data.len().saturating_sub(start));
        if end > start && end <= data.len() {
            chunk[..end - start].copy_from_slice(&data[start..end]);
        }
    }
    FrameLease::single_plane_shared(
        FrameMeta::new(
            MediaFormat::new(FourCc::RGBA, res, ColorSpace::Srgb),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostExternal,
            reason: ResidencyTransitionReason::Capture,
            copied: true,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

/// Convert an FFmpeg RGBA frame into packed `RG24` output.
#[cfg(not(target_os = "linux"))]
pub(crate) fn blit_rgb24_frame(
    rgba: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let stride = rgba.stride(0);
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
            MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::Capture,
            copied: true,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

#[cfg(target_os = "linux")]
// Kept for RGB24 shared-memory paths selected by file/netcam video backends.
#[allow(dead_code)]
pub(crate) fn blit_shared_rgb24_frame(
    rgba: &FfFrame,
    res: Resolution,
    layout: PlaneLayout,
    pool: &SharedBufferPool,
    timestamp: u64,
) -> Result<FrameLease, styx_core::buffer::FrameExportError> {
    let stride = rgba.stride(0);
    let data = rgba.data(0);
    let width = res.width.get() as usize;
    let mut lease = pool.lease()?;
    lease.try_resize(layout.len)?;
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
    FrameLease::single_plane_shared(
        FrameMeta::new(
            MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostExternal,
            reason: ResidencyTransitionReason::Capture,
            copied: true,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

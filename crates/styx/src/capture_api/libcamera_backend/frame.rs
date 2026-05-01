use libcamera::framebuffer::AsFrameBuffer;
use libcamera::framebuffer_allocator::FrameBuffer;
use smallvec::SmallVec;
use styx_core::prelude::*;

use super::backing::{self, BackingPlaneView};
use super::util::plane_height_for_format;
use crate::capture_api::CaptureError;

pub(super) struct CompletedFrameParts {
    pub timestamp: u64,
    pub layouts: SmallVec<[PlaneLayout; 3]>,
    pub plane_views: SmallVec<[BackingPlaneView; 3]>,
}

pub(super) fn completed_frame_parts(
    framebuffer: &FrameBuffer,
    wire_format: MediaFormat,
    active_stride: usize,
) -> Result<CompletedFrameParts, CaptureError> {
    let meta = framebuffer
        .metadata()
        .ok_or_else(|| CaptureError::Backend("libcamera framebuffer metadata missing".into()))?;
    let timestamp = meta.timestamp();
    let planes_meta = meta.planes();
    let framebuffer_planes = framebuffer.planes();
    let height = wire_format.resolution.height.get() as usize;
    let mut layouts = SmallVec::<[PlaneLayout; 3]>::new();
    let mut plane_views = SmallVec::<[BackingPlaneView; 3]>::new();

    let code = wire_format.code;
    let is_nv12 = code == FourCc::NV12 || code == FourCc::NV21;
    if is_nv12
        && !framebuffer_planes.is_empty()
        && let Some(first_plane) = framebuffer_planes.get(0)
        && let Some(first_offset) = first_plane.offset()
    {
        let slice_len = first_plane.len();
        let total_len = planes_meta
            .get(0)
            .map(|m| m.bytes_used as usize)
            .filter(|n| *n > 0)
            .map(|n| n.min(slice_len))
            .unwrap_or(slice_len);

        let width = wire_format.resolution.width.get() as usize;
        let y_height = height;
        let uv_height = height / 2;
        let denom = y_height.saturating_add(uv_height).max(1);
        let inferred = total_len / denom;
        let stride = if active_stride > 0 {
            active_stride
        } else {
            inferred.max(width).max(1)
        };

        let y_len = stride.saturating_mul(y_height);
        let uv_len = stride.saturating_mul(uv_height);
        if y_len.saturating_add(uv_len) <= total_len && uv_height > 0 {
            layouts.push(PlaneLayout {
                offset: 0,
                len: y_len,
                stride,
            });
            layouts.push(PlaneLayout {
                offset: y_len,
                len: uv_len,
                stride,
            });
            plane_views.push(BackingPlaneView {
                fd: first_plane.fd(),
                offset: first_offset,
                len: total_len,
            });
            plane_views.push(BackingPlaneView {
                fd: first_plane.fd(),
                offset: first_offset,
                len: total_len,
            });
        }
    }

    if layouts.is_empty() {
        layouts = planes_meta
            .into_iter()
            .enumerate()
            .map(|(idx, plane_meta)| {
                let slice_len = framebuffer_planes
                    .get(idx)
                    .map(|plane| plane.len())
                    .unwrap_or_default();
                let mut len = plane_meta.bytes_used as usize;
                if len == 0 {
                    len = slice_len;
                } else {
                    len = len.min(slice_len);
                }
                let plane_height = plane_height_for_format(code, idx, height);
                let stride = if idx == 0 && active_stride > 0 {
                    if plane_height == 0 {
                        active_stride
                    } else {
                        let max_stride = slice_len / plane_height;
                        active_stride.min(max_stride.max(1))
                    }
                } else {
                    backing::infer_stride(len, slice_len, plane_height)
                };
                PlaneLayout {
                    offset: 0,
                    len,
                    stride,
                }
            })
            .collect::<SmallVec<[_; 3]>>();

        for idx in 0..framebuffer_planes.len() {
            let Some(plane) = framebuffer_planes.get(idx) else {
                break;
            };
            let Some(offset) = plane.offset() else {
                break;
            };
            plane_views.push(BackingPlaneView {
                fd: plane.fd(),
                offset,
                len: plane.len(),
            });
        }
    }

    if plane_views.len() != layouts.len() {
        return Err(CaptureError::Backend(
            "libcamera plane layout mismatch".into(),
        ));
    }

    Ok(CompletedFrameParts {
        timestamp,
        layouts,
        plane_views,
    })
}

use libcamera::framebuffer::AsFrameBuffer;
use smallvec::SmallVec;
use styx_core::prelude::*;

use super::backing::{self, BackingPlaneView};
use super::util::plane_height_for_format;
use crate::capture_api::CaptureError;

pub(super) struct CompletedFrameParts {
    pub timestamp: u64,
    pub sequence: u32,
    pub layouts: SmallVec<[PlaneLayout; 3]>,
    pub plane_views: SmallVec<[BackingPlaneView; 3]>,
}

pub(super) fn completed_frame_parts(
    framebuffer: &dyn AsFrameBuffer,
    wire_format: MediaFormat,
    active_stride: usize,
) -> Result<CompletedFrameParts, CaptureError> {
    let meta = framebuffer
        .metadata()
        .ok_or_else(|| CaptureError::Backend("libcamera framebuffer metadata missing".into()))?;
    let timestamp = meta.timestamp();
    let sequence = meta.sequence();
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
                    match slice_len.checked_div(plane_height) {
                        None => active_stride,
                        Some(max_stride) => active_stride.min(max_stride.max(1)),
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
        sequence,
        layouts,
        plane_views,
    })
}

/// The clock of libcamera's `SensorTimestamp`. libcamera documents `CLOCK_BOOTTIME`, but on
/// V4L2 pipelines (Raspberry Pi's included) it is the receiver's buffer timestamp, which the
/// kernel takes on `CLOCK_MONOTONIC`. The two differ by the time the system spent suspended:
/// when that is under a second (a device that never suspends: 0) either is right to well
/// under a frame and the documented clock is kept; otherwise the timestamp is on the clock it
/// is not ahead of (a frame is never older than the suspended time).
pub(super) fn sensor_clock(
    timestamp: u64,
    monotonic_now: u64,
    boottime_now: u64,
) -> TimestampClock {
    const SUSPENDED: u64 = 1_000_000_000;
    if boottime_now.saturating_sub(monotonic_now) < SUSPENDED || timestamp > monotonic_now {
        TimestampClock::Boottime
    } else {
        TimestampClock::Monotonic
    }
}

/// What the capture worker knows of a completed request's frame.
#[derive(Clone, Copy, Debug)]
pub(super) struct Completed {
    /// Its sensor timestamp, on `clock`.
    pub timestamp: u64,
    pub clock: TimestampClock,
    /// To the configured timestamp clock (`None`: keep `clock`).
    pub conversion: Option<ClockConversion>,
    pub sequence: u32,
    pub buffer_memory: &'static str,
    /// When the worker took the completed request from libcamera (`Hop::Dequeued`).
    pub dequeued: std::time::Instant,
}

/// The primary frame's metadata: timestamps (converted), backend metadata, sensor latency and
/// its `Dequeued` hop; the queue stamps `Sensor` and `Queued` when it is delivered.
pub(super) fn frame_meta(format: MediaFormat, c: &Completed) -> FrameMeta {
    let mut meta = FrameMeta::new(format, c.timestamp)
        .with_backend(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
            sequence: c.sequence,
            buffer_memory: c.buffer_memory,
        }))
        .with_capture_instant(c.dequeued)
        .with_sensor_latency(c.clock)
        .in_clock(c.clock, c.conversion)
        .with_transition(ResidencyTransition {
            from: FrameResidency::Dmabuf,
            to: FrameResidency::Dmabuf,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        });
    meta.hops
        .set(Hop::Dequeued, CaptureInstant::from(c.dequeued).as_nanos());
    meta
}

/// A companion's metadata (the ISP's second output of the same request): the primary's
/// timestamp, so the two match.
pub(super) fn companion_meta(format: MediaFormat, c: &Completed, sequence: u32) -> FrameMeta {
    FrameMeta::new(format, c.timestamp)
        .with_backend(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
            sequence,
            buffer_memory: c.buffer_memory,
        }))
        .in_clock(c.clock, c.conversion)
}

//! Stream configuration for the primary output and the ISP's optional second output.

use libcamera::framebuffer::AsFrameBuffer;
use styx_core::prelude::{CompanionKind, FourCc, FrameLease};

use super::heap::CaptureBuffer;
use crate::capture_api::CaptureError;

pub(super) fn framebuffer_refs(buffers: &[CaptureBuffer]) -> Vec<&dyn AsFrameBuffer> {
    buffers.iter().map(CaptureBuffer::as_framebuffer).collect()
}

/// Use of the ISP's second processed output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SecondStream {
    None,
    /// PiSP temporal-denoise output at the primary size; emitted instead of the primary.
    Tdn,
    /// Downscaled copy of the primary (`2^-level`) attached as a pyramid companion.
    Pyramid(u8),
}

/// Generate and validate a stream configuration for `code` at `size`, plus the second output.
pub(super) fn configure_streams(
    cam: &libcamera::camera::ActiveCamera<'_>,
    role: libcamera::stream::StreamRole,
    second: SecondStream,
    code: FourCc,
    size: libcamera::geometry::Size,
    buffer_count: u32,
) -> Result<
    (
        libcamera::camera::CameraConfiguration,
        libcamera::camera::CameraConfigurationStatus,
    ),
    CaptureError,
> {
    use libcamera::stream::StreamRole;

    let (second_role, second_size) = match second {
        SecondStream::None => (None, size),
        SecondStream::Tdn => (Some(StreamRole::VideoRecording), size),
        SecondStream::Pyramid(level) => (
            Some(StreamRole::ViewFinder),
            libcamera::geometry::Size::new(
                ((size.width >> level) & !1).max(2),
                ((size.height >> level) & !1).max(2),
            ),
        ),
    };
    let mut roles = vec![role];
    roles.extend(second_role);
    let mut cfgs = cam
        .generate_configuration(&roles)
        .ok_or(CaptureError::LibcameraGenerateConfigurationFailed)?;
    if second_role.is_some() && cfgs.get(1).is_none() {
        return Err(match second {
            SecondStream::Tdn => CaptureError::LibcameraTdnOutputUnavailable,
            _ => CaptureError::Backend("camera has no second processed output".into()),
        });
    }
    for (index, stream_size) in [size, second_size]
        .into_iter()
        .take(roles.len())
        .enumerate()
    {
        let mut cfg = cfgs
            .get_mut(index)
            .ok_or_else(|| CaptureError::Backend("missing stream config".into()))?;
        cfg.set_pixel_format(libcamera::pixel_format::PixelFormat::new(code.to_u32(), 0));
        cfg.set_size(stream_size);
        cfg.set_buffer_count(buffer_count);
    }
    let status = cfgs.validate();
    Ok((cfgs, status))
}

/// Attach the ISP's downscaled second output as a pyramid companion. A companion whose
/// timestamp does not match the primary is dropped rather than failing capture.
pub(super) fn attach_companion(
    frame: FrameLease,
    companion: Option<(u8, FrameLease)>,
    luma_view: bool,
) -> Result<FrameLease, CaptureError> {
    let Some((level, companion)) = companion else {
        return Ok(frame);
    };
    if companion.meta().timestamp != frame.meta().timestamp {
        tracing::debug!(
            backend = "libcamera",
            "pyramid companion timestamp mismatch; dropping companion"
        );
        return Ok(frame);
    }
    let companion = if luma_view {
        companion.into_luma()
    } else {
        Ok(companion)
    };
    companion
        .and_then(|companion| frame.with_companion(CompanionKind::Pyramid { level }, companion))
        .map_err(|err| CaptureError::Backend(err.to_string()))
}

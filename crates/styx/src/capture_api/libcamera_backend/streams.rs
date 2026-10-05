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
    /// The capture at another size, attached as a `CompanionKind::Scaled` companion.
    Scaled(u32, u32),
}

impl SecondStream {
    /// The companion kind this output is attached as.
    pub(super) fn companion_kind(self) -> Option<CompanionKind> {
        match self {
            Self::Pyramid(level) => Some(CompanionKind::Pyramid { level }),
            Self::Scaled(..) => Some(CompanionKind::Scaled),
            Self::None | Self::Tdn => None,
        }
    }
}

/// Use of the second output: TDN when on, else pyramid companions, else a second size.
pub(super) fn choose_second_stream(
    tdn: bool,
    pyramid_level: u8,
    second_output_size: Option<(u32, u32)>,
    emulate_rgb24: bool,
) -> SecondStream {
    let second = if tdn {
        SecondStream::Tdn
    } else if pyramid_level > 0 && !emulate_rgb24 {
        SecondStream::Pyramid(pyramid_level)
    } else if let Some((w, h)) = second_output_size
        && !emulate_rgb24
    {
        SecondStream::Scaled(w, h)
    } else {
        SecondStream::None
    };
    if pyramid_level > 0 && second != SecondStream::Pyramid(pyramid_level) {
        crate::trace::warn!(
            backend = "libcamera",
            pyramid_level,
            tdn_enabled = tdn,
            "libcamera pyramid companion disabled: second output unavailable for this request"
        );
    }
    if second_output_size.is_some() && !matches!(second, SecondStream::Scaled(..)) {
        crate::trace::warn!(
            backend = "libcamera",
            tdn_enabled = tdn,
            pyramid_level,
            "libcamera second output disabled: TDN or the pyramid uses it"
        );
    }
    second
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
        SecondStream::Scaled(width, height) => (
            Some(StreamRole::ViewFinder),
            libcamera::geometry::Size::new((width & !1).max(2), (height & !1).max(2)),
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

/// One request per primary buffer, each with its second output's buffer when there is one.
pub(super) fn build_requests(
    cam: &mut libcamera::camera::ActiveCamera<'_>,
    primary: Vec<CaptureBuffer>,
    second: Option<Vec<CaptureBuffer>>,
    stream: &libcamera::stream::Stream,
    second_stream: Option<&libcamera::stream::Stream>,
) -> Result<Vec<libcamera::request::Request>, CaptureError> {
    let err = |e: std::io::Error| super::util::classify_libcamera_backend_message(e.to_string());
    let mut second = match second_stream {
        Some(_) => match second {
            Some(b) if !b.is_empty() => Some(b.into_iter()),
            _ => return Err(CaptureError::LibcameraTdnOutputUnavailable),
        },
        None => None,
    };
    let mut requests = Vec::new();
    for (i, buf) in primary.into_iter().enumerate() {
        let extra = match (&mut second, second_stream) {
            (Some(it), Some(s)) => match it.next() {
                Some(b) => Some((b, s)),
                // As many requests as both outputs have buffers.
                None => break,
            },
            _ => None,
        };
        let mut req = cam
            .create_request(Some(i as u64))
            .ok_or_else(|| CaptureError::Backend("request create failed".into()))?;
        buf.add_to(&mut req, stream).map_err(err)?;
        if let Some((b, s)) = extra {
            b.add_to(&mut req, s).map_err(err)?;
        }
        requests.push(req);
    }
    Ok(requests)
}

/// Attach the ISP's second output as a companion of `kind`. A companion whose timestamp does
/// not match the primary is dropped rather than failing capture.
pub(super) fn attach_companion(
    frame: FrameLease,
    companion: Option<(CompanionKind, FrameLease)>,
    luma_view: bool,
) -> Result<FrameLease, CaptureError> {
    let Some((kind, companion)) = companion else {
        return Ok(frame);
    };
    if companion.meta().timestamp != frame.meta().timestamp {
        crate::trace::debug!(
            backend = "libcamera",
            "companion timestamp mismatch; dropping companion"
        );
        return Ok(frame);
    }
    let companion = if luma_view {
        companion.into_luma()
    } else {
        Ok(companion)
    };
    companion
        .and_then(|companion| frame.with_companion(kind, companion))
        .map_err(|err| CaptureError::Backend(err.to_string()))
}

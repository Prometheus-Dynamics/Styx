//! The frame processed at full size by the software ISP.

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_settings::IspSettings;
use styx_soft::{Arithmetic, Demosaic, Options, SoftIspStage, YuvMatrix};

use crate::Picture;
use crate::error::StillError;

/// The options a still is processed with: the Malvar-He-Cutler demosaic (the preview uses the
/// bilinear one), BT.601 full range (the PiSP's stills), no statistics.
fn options() -> Options {
    Options {
        demosaic: Demosaic::Mhc,
        yuv: YuvMatrix::Bt601Full,
        arithmetic: Arithmetic::Auto,
        stats: None,
    }
}

/// `frame` processed with `isp` into a picture of format `code` at the frame's size.
pub(crate) fn picture<M>(
    frame: &FrameLease<M>,
    isp: &IspSettings,
    code: FourCc,
) -> Result<Picture, StillError> {
    let input = frame.format();
    let (width, height) = (input.resolution.width.get(), input.resolution.height.get());
    let mut stage = SoftIspStage::new(input.code, width, height, isp, options())?;

    let resolution = Resolution::new(width, height)
        .ok_or_else(|| StillError::Format(format!("{width}x{height}")))?;
    let layouts = packed_layouts(code, width, height, None, None)
        .map_err(|e| StillError::Format(format!("{code:?}: {e:?}")))?;
    let span = layouts
        .span()
        .ok_or_else(|| StillError::Buffer("picture size overflows".into()))?;
    let pool = FramePool::<()>::host(1, BufferSize::new(span, 0))
        .map_err(|e| StillError::Buffer(format!("{e:?}")))?;
    let entry = FrameEntry::new(
        frame.id(),
        frame.timestamp_ns(),
        MediaFormat::with_defaults(code, resolution),
        layouts,
    );
    let mut out = pool
        .acquire(entry)
        .map_err(|e| StillError::Buffer(format!("{e:?}")))?;

    stage.process(frame, &mut out, None)?;
    let read = out.read().ok_or(StillError::NotReadable)?;
    Ok(Picture {
        code,
        width,
        height,
        data: read.bytes()[..span].to_vec(),
    })
}

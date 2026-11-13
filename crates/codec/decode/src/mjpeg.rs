//! MJPEG (each frame one JPEG) to RGB24, with zune-jpeg (pure Rust) or libturbojpeg.

use styx_core::{Backend, Codec, CodecDescriptor, CodecError, CodecKind, Decoder};
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, packed_layouts};
use zune_core::bytestream::ZCursor;
use zune_jpeg::JpegDecoder;

use crate::turbo::TurboDecoder;

/// Which library decodes the JPEGs.
#[derive(Debug)]
enum Engine {
    Zune,
    Turbo(TurboDecoder),
}

/// One JPEG per frame, in, RGB24 out. Built by [`MjpegDecoder::zune`] or
/// [`MjpegDecoder::turbojpeg`].
#[derive(Debug)]
pub struct MjpegDecoder {
    descriptor: CodecDescriptor,
    engine: Engine,
}

/// The descriptor of an MJPEG decoder from `input` (`MJPG` or `JPEG`) under `name`.
pub(crate) fn descriptor(input: FourCc, name: &'static str) -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Decoder,
        input,
        output: FourCc::RG24,
        backend: Backend::Native,
        name,
    }
}

impl MjpegDecoder {
    /// zune-jpeg, pure Rust, always available. `input` is `MJPG` or `JPEG`.
    pub fn zune(input: FourCc) -> Self {
        Self {
            descriptor: descriptor(input, "mjpeg-zune"),
            engine: Engine::Zune,
        }
    }

    /// libturbojpeg. Fails with `Unavailable` when the library was not found at build time.
    pub fn turbojpeg(input: FourCc) -> Result<Self, CodecError> {
        Ok(Self {
            descriptor: descriptor(input, "mjpeg-turbojpeg"),
            engine: Engine::Turbo(TurboDecoder::new()?),
        })
    }
}

/// Dev's check: the decoded size must be non-empty and at most four times the declared stream
/// size (or 4096 pixels, whichever is larger). A header claiming more is refused before any
/// output is sized from it.
pub(crate) fn check_decoded_size(
    declared: Resolution,
    width: usize,
    height: usize,
) -> Result<(), CodecError> {
    let pixels = width as u64 * height as u64;
    let limit = 4 * u64::from(declared.width.get()) * u64::from(declared.height.get());
    if pixels == 0 || pixels > limit.max(4096) {
        return Err(CodecError::Rejected(format!(
            "compressed frame claims {width}x{height}, far larger than its {}x{} stream",
            declared.width, declared.height
        )));
    }
    Ok(())
}

/// Sizes the output lease for `width` x `height` RGB24 (packed rows, one plane) and sets its
/// entry from the input's id and timestamp. Must run before `output.write()`.
fn begin_rgb<M>(
    input: &FrameLease<M>,
    output: &mut FrameLease<M>,
    code: FourCc,
    width: usize,
    height: usize,
    color: ColorSpace,
    range: ColorRange,
) -> Result<(), CodecError> {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(CodecError::Rejected("output size does not fit".into()));
    };
    let res = Resolution::new(w, h).ok_or_else(|| CodecError::Rejected("empty output".into()))?;
    let layouts =
        packed_layouts(code, w, h, None, None).map_err(|e| CodecError::Rejected(e.to_string()))?;
    output
        .set_entry(FrameEntry::new(
            input.id(),
            input.timestamp_ns(),
            MediaFormat::new(code, res, color, range),
            layouts,
        ))
        .map_err(|e| CodecError::Rejected(format!("output lease: {e}")))
}

impl<M> Codec<M> for MjpegDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Decoder<M> for MjpegDecoder {
    fn decode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let code = input.format().code;
        if code != self.descriptor.input {
            return Err(CodecError::Rejected(format!(
                "expected {}, got {code}",
                self.descriptor.input
            )));
        }
        let declared = input.format().resolution;
        let (color, fmt_range) = (input.format().color, input.format().range);
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("input frame is not readable".into()))?;
        let jpeg = read
            .plane(0)
            .ok_or_else(|| CodecError::Rejected("MJPEG frame has no plane".into()))?
            .data;

        match &mut self.engine {
            Engine::Zune => {
                let mut dec = JpegDecoder::new(ZCursor::new(jpeg));
                dec.decode_headers()
                    .map_err(|e| CodecError::Rejected(format!("jpeg header: {e}")))?;
                let (w, h) = dec
                    .dimensions()
                    .ok_or_else(|| CodecError::Rejected("jpeg header has no size".into()))?;
                check_decoded_size(declared, w, h)?;
                begin_rgb(input, output, FourCc::RG24, w, h, color, fmt_range)?;
                let mut planes = output
                    .write()
                    .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
                let plane = planes
                    .plane(0)
                    .ok_or_else(|| CodecError::Failed("output has no plane".into()))?;
                if dec.output_buffer_size() != Some(plane.data.len()) {
                    return Err(CodecError::Rejected(
                        "jpeg output is not packed RGB24 (a grey or CMYK JPEG?)".into(),
                    ));
                }
                dec.decode_into(plane.data)
                    .map_err(|e| CodecError::Rejected(format!("jpeg decode: {e}")))
            }
            Engine::Turbo(turbo) => {
                let (w, h) = turbo.header(jpeg)?;
                check_decoded_size(declared, w, h)?;
                begin_rgb(input, output, FourCc::RG24, w, h, color, fmt_range)?;
                let mut planes = output
                    .write()
                    .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
                let plane = planes
                    .plane(0)
                    .ok_or_else(|| CodecError::Failed("output has no plane".into()))?;
                turbo.decompress(jpeg, plane.data, w, h)
            }
        }
    }
}

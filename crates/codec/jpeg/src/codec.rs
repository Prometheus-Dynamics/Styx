//! [`StyxJpegEncoder`]: the encoder as a `styx-codec-api` [`Encoder`]: planar frames in, MJPG
//! frames out, on the caller's pool.

use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Encoder, Implementation,
};
use styx_format::{FourCc, MediaFormat};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};

use crate::encoder::{JpegEncoder, PlanarInput};
use crate::entropy::Fixed;

/// The planar layouts the encoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Planes {
    I420,
    Nv12,
    Gray,
}

fn planes_of(input: FourCc) -> Option<Planes> {
    match input {
        FourCc::I420 | FourCc::YU12 => Some(Planes::I420),
        FourCc::NV12 => Some(Planes::Nv12),
        FourCc::GREY | FourCc::R8 => Some(Planes::Gray),
        _ => None,
    }
}

/// The input formats [`StyxJpegEncoder`] takes: I420 (YU12), NV12, GREY and R8.
pub const INPUTS: [FourCc; 5] = [
    FourCc::I420,
    FourCc::YU12,
    FourCc::NV12,
    FourCc::GREY,
    FourCc::R8,
];

/// The descriptor of an encoder from `input` to MJPG on the native backend, `None` for a format
/// it does not take.
pub fn descriptor(input: FourCc) -> Option<CodecDescriptor> {
    planes_of(input)?;
    Some(CodecDescriptor {
        kind: CodecKind::Encoder,
        input,
        output: FourCc::MJPG,
        backend: Backend::Native,
        name: "styx-jpeg",
    })
}

/// Styx's own JPEG encoder as an [`Encoder`] for planar frames of one format.
#[derive(Debug)]
pub struct StyxJpegEncoder {
    descriptor: CodecDescriptor,
    planes: Planes,
    jpeg: JpegEncoder,
}

impl StyxJpegEncoder {
    /// An encoder for frames of `input` at `quality` (1 to 100). `Rejected` for a format it does
    /// not take.
    pub fn new(input: FourCc, quality: u8) -> Result<Self, CodecError> {
        let (Some(descriptor), Some(planes)) = (descriptor(input), planes_of(input)) else {
            return Err(CodecError::Rejected(format!(
                "styx-jpeg does not take {input}"
            )));
        };
        Ok(Self {
            descriptor,
            planes,
            jpeg: JpegEncoder::new(quality),
        })
    }

    /// Restart markers after every MCU row (see [`JpegEncoder::with_restart_rows`]).
    pub fn with_restart_rows(mut self, on: bool) -> Self {
        self.jpeg = self.jpeg.with_restart_rows(on);
        self
    }

    /// The underlying encoder (its quality, backend and quantisers).
    pub fn jpeg(&self) -> &JpegEncoder {
        &self.jpeg
    }
}

/// A registry candidate for frames of `input` at `quality`, `None` for a format it does not take.
pub fn candidate<M: 'static>(input: FourCc, quality: u8) -> Option<Candidate<M>> {
    let descriptor = descriptor(input)?;
    Some(Candidate::new(descriptor, move || {
        Ok(Implementation::Encoder(Box::new(StyxJpegEncoder::new(
            input, quality,
        )?)))
    }))
}

impl<M> Codec<M> for StyxJpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Encoder<M> for StyxJpegEncoder {
    fn encode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let format = *input.format();
        if format.code != self.descriptor.input {
            return Err(CodecError::Rejected(format!(
                "styx-jpeg encodes {}, not {}",
                self.descriptor.input, format.code
            )));
        }
        let (width, height) = (
            format.resolution.width.get() as usize,
            format.resolution.height.get() as usize,
        );
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("input frame is not readable".into()))?;
        let plane = |i: usize| {
            read.plane(i)
                .ok_or_else(|| CodecError::Rejected(format!("input frame has no plane {i}")))
        };
        let pixels = match self.planes {
            Planes::I420 => {
                let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
                PlanarInput::I420 {
                    y: y.data,
                    u: u.data,
                    v: v.data,
                    width,
                    height,
                    y_stride: y.stride,
                    c_stride: u.stride,
                }
            }
            Planes::Nv12 => {
                let (y, uv) = (plane(0)?, plane(1)?);
                PlanarInput::Nv12 {
                    y: y.data,
                    uv: uv.data,
                    width,
                    height,
                    y_stride: y.stride,
                    uv_stride: uv.stride,
                }
            }
            Planes::Gray => {
                let y = plane(0)?;
                PlanarInput::Gray {
                    y: y.data,
                    width,
                    height,
                    stride: y.stride,
                }
            }
        };
        let mut write = output
            .write()
            .ok_or_else(|| CodecError::Failed("output frame is shared or not writable".into()))?;
        let dst = write
            .plane(0)
            .ok_or_else(|| CodecError::Rejected("output frame has no plane".into()))?;
        let mut sink = Fixed::new(dst.data);
        self.jpeg.run(&pixels, &mut sink)?;
        let len = sink
            .written()
            .ok_or_else(|| CodecError::Rejected("the JPEG does not fit the output frame".into()))?;
        let layouts = Layouts::new(&[PlaneLayout::new(0, len, len)])
            .ok_or_else(|| CodecError::Failed("one plane does not fit a layout".into()))?;
        let entry = FrameEntry::new(
            input.id(),
            input.timestamp_ns(),
            MediaFormat::new(FourCc::MJPG, format.resolution, format.color, format.range),
            layouts,
        );
        output
            .set_entry(entry)
            .map_err(|error| CodecError::Rejected(error.to_string()))
    }
}

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, frame_to_dynamic_image};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use styx_core::prelude::*;

/// Simple passthrough decoder that validates FourCc and returns the frame unchanged.
pub struct PassthroughDecoder {
    descriptor: CodecDescriptor,
}

impl PassthroughDecoder {
    /// Create a passthrough decoder for the given FourCc.
    pub fn new(fourcc: FourCc) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: fourcc,
                output: fourcc,
                name: "passthrough",
                impl_name: "passthrough",
            },
        }
    }
}

impl Codec for PassthroughDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        if input.meta().format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: input.meta().format.code,
            });
        }
        Ok(input)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for PassthroughDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        frame_to_dynamic_image(&frame).ok_or_else(|| {
            CodecError::Codec("unable to convert passthrough frame to DynamicImage".into())
        })
    }
}

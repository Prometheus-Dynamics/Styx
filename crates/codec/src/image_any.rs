use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// Generic decoder using the `image` crate to handle common formats (JPEG/PNG/etc).
pub struct ImageAnyDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl ImageAnyDecoder {
    /// Output FourCc (e.g. RGBA).
    pub fn new(output: FourCc) -> Self {
        Self::with_pool(output, BufferPool::with_limits(2, 1 << 20, 4))
    }

    pub fn with_pool(output: FourCc, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"ANY "),
                output,
                name: "image",
                impl_name: "image-crate",
            },
            pool,
        }
    }
}

impl Codec for ImageAnyDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("image frame missing plane".into()))?;

        let dynimg = image::load_from_memory(plane.data())
            .map_err(|e| CodecError::Codec(e.to_string()))?
            .into_rgba8();
        let (width, height) = dynimg.dimensions();
        let resolution = Resolution::new(width, height)
            .ok_or_else(|| CodecError::Codec("invalid image dimensions".into()))?;
        let format = MediaFormat::new(self.descriptor.output, resolution, ColorSpace::Srgb);
        let layout = plane_layout_from_dims(resolution.width, resolution.height, 4);

        let mut buf = self.pool.lease();
        buf.resize(layout.len);
        buf.as_mut_slice().copy_from_slice(dynimg.as_raw());

        Ok(FrameLease::single_plane(
            FrameMeta::new(format, input.meta().timestamp),
            buf,
            layout.len,
            layout.stride,
        ))
    }
}

#[cfg(feature = "image")]
impl ImageDecode for ImageAnyDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

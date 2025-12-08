use styx_core::prelude::*;
use turbojpeg::Image as TjImage;
use turbojpeg::{Decompressor, PixelFormat as TjPixelFormat};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// MJPEG decoder using libturbojpeg.
pub struct TurbojpegDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl TurbojpegDecoder {
    pub fn new(output: FourCc) -> Self {
        Self::with_pool(output, BufferPool::with_limits(2, 1 << 20, 4))
    }

    pub fn with_pool(output: FourCc, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"MJPG"),
                output,
                name: "mjpeg",
                impl_name: "turbojpeg",
            },
            pool,
        }
    }
}

impl Codec for TurbojpegDecoder {
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mjpeg frame missing plane".into()))?;

        let mut tj = Decompressor::new().map_err(|e| CodecError::Codec(e.to_string()))?;
        let header = tj
            .read_header(plane.data())
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        let resolution = Resolution::new(header.width as u32, header.height as u32)
            .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        let layout = plane_layout_from_dims(resolution.width, resolution.height, 3);

        let mut buf = self.pool.lease();
        buf.resize(layout.len);
        let mut image = TjImage {
            pixels: buf.as_mut_slice(),
            width: header.width,
            pitch: layout.stride,
            height: header.height,
            format: TjPixelFormat::RGB,
        };
        tj.decompress(plane.data(), image.as_deref_mut())
            .map_err(|e| CodecError::Codec(e.to_string()))?;

        Ok(FrameLease::single_plane(
            FrameMeta::new(format, input.meta().timestamp),
            buf,
            layout.len,
            layout.stride,
        ))
    }
}

#[cfg(feature = "image")]
impl ImageDecode for TurbojpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

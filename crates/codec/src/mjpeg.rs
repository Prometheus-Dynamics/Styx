use std::io::Cursor;

use jpeg_decoder::{Decoder, PixelFormat};
use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// MJPEG decoder that produces RGB24 frames using a reusable buffer pool.
pub struct MjpegDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl MjpegDecoder {
    /// Create a decoder that outputs the given FourCc (e.g. `FourCc::new(*b"RG24")`).
    pub fn new(output: FourCc) -> Self {
        Self::with_pool(output, BufferPool::with_limits(2, 1 << 20, 4))
    }

    /// Create a decoder for a specific input FourCc (e.g. `MJPG` or `JPEG`).
    pub fn new_for_input(input: FourCc, output: FourCc) -> Self {
        Self::with_pool_for_input(input, output, BufferPool::with_limits(2, 1 << 20, 4))
    }

    /// Create a decoder with a caller-provided buffer pool.
    pub fn with_pool(output: FourCc, pool: BufferPool) -> Self {
        Self::with_pool_for_input(FourCc::new(*b"MJPG"), output, pool)
    }

    /// Create a decoder for a specific input FourCc using a caller-provided buffer pool.
    pub fn with_pool_for_input(input: FourCc, output: FourCc, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output,
                name: "mjpeg",
                impl_name: "jpeg-decoder",
            },
            pool,
        }
    }
}

impl Codec for MjpegDecoder {
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

        let mut decoder = Decoder::new(Cursor::new(plane.data()));
        let pixels = decoder
            .decode()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        let info = decoder
            .info()
            .ok_or_else(|| CodecError::Codec("mjpeg missing info".into()))?;

        if info.pixel_format != PixelFormat::RGB24 {
            return Err(CodecError::Codec(format!(
                "unsupported MJPEG pixel format {:?}",
                info.pixel_format
            )));
        }

        let resolution = Resolution::new(info.width as u32, info.height as u32)
            .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        let layout = plane_layout_from_dims(resolution.width, resolution.height, 3);

        let mut buf = self.pool.lease();
        buf.resize(layout.len);
        let copy_len = pixels.len().min(layout.len);
        buf.as_mut_slice()[..copy_len].copy_from_slice(&pixels[..copy_len]);

        Ok(FrameLease::single_plane(
            FrameMeta::new(format, input.meta().timestamp),
            buf,
            layout.len,
            layout.stride,
        ))
    }
}

#[cfg(feature = "image")]
impl ImageDecode for MjpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

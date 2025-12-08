use styx_core::prelude::*;

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// MJPEG encoder using mozjpeg.
pub struct MozjpegEncoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    quality: i32,
}

impl MozjpegEncoder {
    pub fn new(input: FourCc, quality: i32) -> Self {
        Self::with_pool(input, quality, BufferPool::with_limits(2, 1 << 20, 4))
    }

    pub fn with_pool(input: FourCc, quality: i32, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output: FourCc::new(*b"MJPG"),
                name: "mjpeg",
                impl_name: "mozjpeg",
            },
            pool,
            quality: quality.clamp(1, 100),
        }
    }
}

impl Codec for MozjpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        // Expect RGB24 input.
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("encoder frame missing plane".into()))?;
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 3);

        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        comp.set_size(width, height);
        comp.set_quality(self.quality as f32);
        let mut dest = comp
            .start_compress(Vec::new())
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        for y in 0..height {
            let line = &plane.data()[y * stride..];
            dest.write_scanlines(&line[..width * 3])
                .map_err(|e| CodecError::Codec(e.to_string()))?;
        }
        let jpeg = dest
            .finish()
            .map_err(|e| CodecError::Codec(e.to_string()))?;

        let mut buf = self.pool.lease();
        buf.resize(jpeg.len());
        buf.as_mut_slice()[..jpeg.len()].copy_from_slice(&jpeg);

        let layout = PlaneLayout {
            offset: 0,
            len: jpeg.len(),
            stride: jpeg.len(), // stride not meaningful for compressed payload
        };
        Ok(FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(
                    self.descriptor.output,
                    meta.format.resolution,
                    meta.format.color,
                ),
                meta.timestamp,
            ),
            buf,
            layout.len,
            layout.stride,
        ))
    }
}

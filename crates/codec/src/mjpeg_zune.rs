use styx_core::prelude::*;
use zune_core::bytestream::ZCursor;
use zune_jpeg::JpegDecoder;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{
    Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_CHUNK_BYTES,
    DEFAULT_CODEC_POOL_SPARE,
};

/// MJPEG decoder using the pure-Rust zune-jpeg backend.
pub struct ZuneMjpegDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl ZuneMjpegDecoder {
    pub fn new(output: FourCc) -> Self {
        Self::with_pool(
            output,
            BufferPool::lazy(DEFAULT_CODEC_POOL_CHUNK_BYTES, DEFAULT_CODEC_POOL_SPARE),
        )
    }

    pub fn with_pool(output: FourCc, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::MJPG,
                output,
                name: "mjpeg",
                impl_name: "zune-jpeg",
            },
            pool,
        }
    }
}

impl Codec for ZuneMjpegDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
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

        let mut decoder = JpegDecoder::new(ZCursor::new(plane.data()));
        let pixels = decoder
            .decode()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        let info = decoder
            .info()
            .ok_or_else(|| CodecError::Codec("jpeg missing info".into()))?;

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

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
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

        let mut decoder = JpegDecoder::new(ZCursor::new(plane.data()));
        let pixels = decoder
            .decode()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        let info = decoder
            .info()
            .ok_or_else(|| CodecError::Codec("jpeg missing info".into()))?;

        let resolution = Resolution::new(info.width as u32, info.height as u32)
            .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        let layout = plane_layout_from_dims(resolution.width, resolution.height, 3);
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(layout.len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        let copy_len = pixels.len().min(layout.len);
        lease.as_mut_slice()[..copy_len].copy_from_slice(&pixels[..copy_len]);

        FrameLease::single_plane_shared(
            FrameMeta::new(format, input.meta().timestamp),
            lease,
            layout.len,
            layout.stride,
        )
        .map(Some)
        .map_err(|err| CodecError::Codec(err.to_string()))
    }
}

#[cfg(feature = "image")]
impl ImageDecode for ZuneMjpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

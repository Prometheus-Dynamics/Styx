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

    fn check_input(&self, input: &FrameLease) -> Result<(), CodecError> {
        if input.meta().format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: input.meta().format.code,
            });
        }
        Ok(())
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
        self.check_input(&input)?;
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mjpeg frame missing plane".into()))?;
        let mut decoder = read_header(plane.data(), input.meta().format.resolution)?;
        let (resolution, len, stride) = output_layout(&decoder)?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        // Decode straight into the leased buffer: no intermediate Vec and no copy.
        let mut buf = self.pool.lease_sized(len);
        decode_pixels(&mut decoder, buf.as_mut_slice())?;

        Ok(FrameLease::single_plane(
            FrameMeta::new(format, input.meta().timestamp),
            buf,
            len,
            stride,
        ))
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.check_input(input)?;
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mjpeg frame missing plane".into()))?;
        let mut decoder = read_header(plane.data(), input.meta().format.resolution)?;
        let (resolution, len, stride) = output_layout(&decoder)?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        decode_pixels(&mut decoder, lease.as_mut_slice())?;

        FrameLease::single_plane_shared(
            FrameMeta::new(format, input.meta().timestamp),
            lease,
            len,
            stride,
        )
        .map(Some)
        .map_err(|err| CodecError::Codec(err.to_string()))
    }
}

/// Parses the JPEG headers and validates the claimed size against the declared stream size.
fn read_header(
    data: &[u8],
    declared: Resolution,
) -> Result<JpegDecoder<ZCursor<&[u8]>>, CodecError> {
    let mut decoder = JpegDecoder::new(ZCursor::new(data));
    decoder
        .decode_headers()
        .map_err(|e| CodecError::Codec(e.to_string()))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| CodecError::Codec("jpeg missing info".into()))?;
    crate::check_decoded_size(declared, width as u32, height as u32)?;
    Ok(decoder)
}

/// Output resolution plus packed RGB `(len, stride)` for a decoder with headers decoded.
fn output_layout(
    decoder: &JpegDecoder<ZCursor<&[u8]>>,
) -> Result<(Resolution, usize, usize), CodecError> {
    let info = decoder
        .info()
        .ok_or_else(|| CodecError::Codec("jpeg missing info".into()))?;
    let resolution = Resolution::new(info.width as u32, info.height as u32)
        .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
    let layout = plane_layout_from_dims(resolution.width, resolution.height, 3);
    if decoder.output_buffer_size() != Some(layout.len) {
        return Err(CodecError::Codec(
            "jpeg output size does not match packed RGB layout".into(),
        ));
    }
    Ok((resolution, layout.len, layout.stride))
}

fn decode_pixels(
    decoder: &mut JpegDecoder<ZCursor<&[u8]>>,
    out: &mut [u8],
) -> Result<(), CodecError> {
    decoder
        .decode_into(out)
        .map_err(|e| CodecError::Codec(e.to_string()))
}

#[cfg(feature = "image")]
impl ImageDecode for ZuneMjpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

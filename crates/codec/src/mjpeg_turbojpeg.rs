use std::sync::Mutex;

use styx_core::prelude::*;
use turbojpeg::Image as TjImage;
use turbojpeg::{
    Compressor, Decompressor, OutputBuf, PixelFormat as TjPixelFormat, Subsamp as TjSubsamp,
};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
#[cfg(target_os = "linux")]
use crate::shared_packet_frame;
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

#[derive(Debug)]
struct TurbojpegEncoderState {
    compressor: Compressor,
}

/// MJPEG encoder using libturbojpeg.
pub struct TurbojpegEncoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    quality: i32,
    state: Mutex<Option<TurbojpegEncoderState>>,
}

impl TurbojpegEncoder {
    pub fn new(input: FourCc, quality: i32) -> Self {
        Self::with_pool(input, quality, BufferPool::lazy(1 << 20, 4))
    }

    pub fn with_pool(input: FourCc, quality: i32, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output: FourCc::new(*b"MJPG"),
                name: "mjpeg",
                impl_name: "turbojpeg",
            },
            pool,
            quality: quality.clamp(1, 100),
            state: Mutex::new(None),
        }
    }

    fn encode_packed(
        &self,
        meta: &FrameMeta,
        pixels: &[u8],
        pitch: usize,
        format: TjPixelFormat,
        subsamp: TjSubsamp,
    ) -> Result<FrameLease, CodecError> {
        let width = meta.format.resolution.width.get().max(1) as usize;
        let height = meta.format.resolution.height.get().max(1) as usize;
        let required = pitch
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("turbojpeg input stride overflow".into()))?;
        if pixels.len() < required {
            return Err(CodecError::Codec(
                "turbojpeg input frame shorter than declared stride".into(),
            ));
        }

        let mut guard = self
            .state
            .lock()
            .map_err(|_| CodecError::Codec("turbojpeg encoder mutex poisoned".into()))?;
        if guard.is_none() {
            let mut compressor =
                Compressor::new().map_err(|err| CodecError::Codec(err.to_string()))?;
            compressor
                .set_quality(self.quality)
                .map_err(|err| CodecError::Codec(err.to_string()))?;
            compressor
                .set_optimize(false)
                .map_err(|err| CodecError::Codec(err.to_string()))?;
            *guard = Some(TurbojpegEncoderState { compressor });
        }
        let state = guard
            .as_mut()
            .ok_or_else(|| CodecError::Codec("turbojpeg encoder unavailable".into()))?;
        let mut output = OutputBuf::new_owned();
        state
            .compressor
            .set_subsamp(subsamp)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        let view = TjImage {
            pixels: &pixels[..required],
            width,
            pitch,
            height,
            format,
        };
        state
            .compressor
            .compress(view, &mut output)
            .map_err(|err| CodecError::Codec(err.to_string()))?;

        let encoded = &output[..];
        let mut buf = self.pool.lease();
        buf.resize(encoded.len());
        buf.as_mut_slice()[..encoded.len()].copy_from_slice(encoded);
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
            encoded.len(),
            encoded.len(),
        ))
    }

    #[cfg(target_os = "linux")]
    fn encode_packed_shared(
        &self,
        meta: &FrameMeta,
        pixels: &[u8],
        pitch: usize,
        format: TjPixelFormat,
        subsamp: TjSubsamp,
        pool: &SharedBufferPool,
    ) -> Result<FrameLease, CodecError> {
        let width = meta.format.resolution.width.get().max(1) as usize;
        let height = meta.format.resolution.height.get().max(1) as usize;
        let required = pitch
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("turbojpeg input stride overflow".into()))?;
        if pixels.len() < required {
            return Err(CodecError::Codec(
                "turbojpeg input frame shorter than declared stride".into(),
            ));
        }

        let mut guard = self
            .state
            .lock()
            .map_err(|_| CodecError::Codec("turbojpeg encoder mutex poisoned".into()))?;
        if guard.is_none() {
            let mut compressor =
                Compressor::new().map_err(|err| CodecError::Codec(err.to_string()))?;
            compressor
                .set_quality(self.quality)
                .map_err(|err| CodecError::Codec(err.to_string()))?;
            compressor
                .set_optimize(false)
                .map_err(|err| CodecError::Codec(err.to_string()))?;
            *guard = Some(TurbojpegEncoderState { compressor });
        }
        let state = guard
            .as_mut()
            .ok_or_else(|| CodecError::Codec("turbojpeg encoder unavailable".into()))?;
        let mut output = OutputBuf::new_owned();
        state
            .compressor
            .set_subsamp(subsamp)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        let view = TjImage {
            pixels: &pixels[..required],
            width,
            pitch,
            height,
            format,
        };
        state
            .compressor
            .compress(view, &mut output)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        shared_packet_frame(&self.descriptor, meta, &output, pool)
    }
}

impl Codec for TurbojpegEncoder {
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("turbojpeg frame missing plane".into()))?;
        let width = meta.format.resolution.width.get().max(1) as usize;
        match &meta.format.code.to_u32().to_le_bytes() {
            b"R8  " | b"GREY" => self.encode_packed(
                meta,
                plane.data(),
                plane.stride().max(width),
                TjPixelFormat::GRAY,
                TjSubsamp::Gray,
            ),
            b"RG24" => self.encode_packed(
                meta,
                plane.data(),
                plane.stride().max(width * 3),
                TjPixelFormat::RGB,
                TjSubsamp::Sub2x2,
            ),
            b"RGBA" => self.encode_packed(
                meta,
                plane.data(),
                plane.stride().max(width * 4),
                TjPixelFormat::RGBA,
                TjSubsamp::Sub2x2,
            ),
            _ => Err(CodecError::Codec(format!(
                "unsupported turbojpeg encoder input {}",
                meta.format.code
            ))),
        }
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("turbojpeg frame missing plane".into()))?;
        let width = meta.format.resolution.width.get().max(1) as usize;
        let frame = match &meta.format.code.to_u32().to_le_bytes() {
            b"R8  " | b"GREY" => self.encode_packed_shared(
                meta,
                plane.data(),
                plane.stride().max(width),
                TjPixelFormat::GRAY,
                TjSubsamp::Gray,
                pool,
            )?,
            b"RG24" => self.encode_packed_shared(
                meta,
                plane.data(),
                plane.stride().max(width * 3),
                TjPixelFormat::RGB,
                TjSubsamp::Sub2x2,
                pool,
            )?,
            b"RGBA" => self.encode_packed_shared(
                meta,
                plane.data(),
                plane.stride().max(width * 4),
                TjPixelFormat::RGBA,
                TjSubsamp::Sub2x2,
                pool,
            )?,
            _ => {
                return Err(CodecError::Codec(format!(
                    "unsupported turbojpeg encoder input {}",
                    meta.format.code
                )));
            }
        };
        Ok(Some(frame))
    }
}

/// MJPEG decoder using libturbojpeg.
pub struct TurbojpegDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl TurbojpegDecoder {
    pub fn new(output: FourCc) -> Self {
        Self::with_pool(output, BufferPool::lazy(1 << 20, 4))
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
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(layout.len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        let mut image = TjImage {
            pixels: lease.as_mut_slice(),
            width: header.width,
            pitch: layout.stride,
            height: header.height,
            format: TjPixelFormat::RGB,
        };
        tj.decompress(plane.data(), image.as_deref_mut())
            .map_err(|e| CodecError::Codec(e.to_string()))?;

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
impl ImageDecode for TurbojpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turbojpeg_encoder_encodes_gray_frames() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::new(*b"GREY"), res, ColorSpace::Unknown);
        let mut buf = BufferPool::with_limits(1, 4, 1).lease();
        buf.resize(4);
        buf.as_mut_slice().copy_from_slice(&[0, 64, 128, 255]);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), buf, 4, 2);

        let encoded = TurbojpegEncoder::new(FourCc::new(*b"GREY"), 85)
            .process(frame)
            .expect("encode frame");
        let plane = encoded.planes().into_iter().next().expect("encoded plane");

        assert_eq!(encoded.meta().format.code, FourCc::new(*b"MJPG"));
        assert_eq!(encoded.meta().timestamp, 7);
        assert!(!plane.data().is_empty());
    }
}

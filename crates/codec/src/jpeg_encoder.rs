use styx_core::prelude::*;

#[cfg(target_os = "linux")]
use crate::shared_packet_frame;
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// MJPEG encoder using mozjpeg.
pub struct MozjpegEncoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    quality: i32,
}

impl MozjpegEncoder {
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
        shared_packet_frame(&self.descriptor, meta, &jpeg, pool).map(Some)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn mozjpeg_shared_output_exports_memfd_packet() {
        let res = Resolution::new(2, 2).unwrap();
        let fmt = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
        let mut buf = BufferPool::with_limits(1, 12, 1).lease();
        buf.resize(12);
        buf.as_mut_slice()
            .copy_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 11), buf, 12, 6);
        let pool = SharedBufferPool::with_capacity(1, 4096).unwrap();
        let encoded = MozjpegEncoder::new(FourCc::new(*b"RG24"), 85)
            .process_shared(&frame, &pool)
            .expect("shared encode")
            .expect("shared frame");

        assert_eq!(encoded.meta().format.code, FourCc::new(*b"MJPG"));
        assert_eq!(encoded.residency(), FrameResidency::CompressedPacket);
        let (_, export) = encoded.export_descriptor_and_backing().expect("export");
        let FrameBackingExport::Memfd { len, .. } = export else {
            panic!("expected memfd export");
        };
        assert!(len > 0);
    }
}

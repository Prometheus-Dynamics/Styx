use std::sync::Mutex;

use styx_core::prelude::*;
use turbojpeg::Image as TjImage;
use turbojpeg::{
    Compressor, OutputBuf, PixelFormat as TjPixelFormat, Subsamp as TjSubsamp, YuvPlanesImage,
};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::mjpeg_turbojpeg_luma::LumaDecodeScale;
#[cfg(target_os = "linux")]
use crate::shared_packet_frame;
use crate::turbojpeg_raw::TjDecompressor;
use crate::{
    Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_CHUNK_BYTES,
    DEFAULT_CODEC_POOL_SPARE,
};

#[derive(Debug)]
struct TurbojpegEncoderState {
    compressor: Compressor,
    /// Chroma (and for YUYV luma) planes split out of interleaved input.
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

/// MJPEG encoder using libturbojpeg.
///
/// Inputs: GREY/R8, RG24, RGBA (packed; turbojpeg converts to YCbCr), and NV12 and YUYV, encoded
/// from their YUV samples without a colour conversion (NV12 as 4:2:0, YUYV as 4:2:2; only the
/// interleaved chroma, and YUYV's luma, are split into planes first).
pub struct TurbojpegEncoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    quality: i32,
    state: Mutex<Option<TurbojpegEncoderState>>,
}

impl TurbojpegEncoder {
    pub fn new(input: FourCc, quality: i32) -> Self {
        Self::with_pool(
            input,
            quality,
            BufferPool::lazy(DEFAULT_CODEC_POOL_CHUNK_BYTES, DEFAULT_CODEC_POOL_SPARE),
        )
    }

    pub fn with_pool(input: FourCc, quality: i32, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output: FourCc::MJPG,
                name: "mjpeg",
                impl_name: "turbojpeg",
            },
            pool,
            quality: quality.clamp(1, 100),
            state: Mutex::new(None),
        }
    }

    /// Compresses `input` and hands the JPEG bytes to `finish`.
    fn encode<R>(
        &self,
        input: &FrameLease,
        finish: impl FnOnce(&FrameMeta, &[u8]) -> Result<R, CodecError>,
    ) -> Result<R, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let planes = input.planes();
        let plane = planes
            .first()
            .ok_or_else(|| CodecError::Codec("turbojpeg frame missing plane".into()))?;
        let width = meta.format.resolution.width.get().max(1) as usize;
        let height = meta.format.resolution.height.get().max(1) as usize;
        let err = |e: turbojpeg::Error| CodecError::Codec(e.to_string());

        let mut guard = self
            .state
            .lock()
            .map_err(|_| CodecError::Codec("turbojpeg encoder mutex poisoned".into()))?;
        if guard.is_none() {
            let mut compressor = Compressor::new().map_err(err)?;
            compressor.set_quality(self.quality).map_err(err)?;
            compressor.set_optimize(false).map_err(err)?;
            *guard = Some(TurbojpegEncoderState {
                compressor,
                y: Vec::new(),
                u: Vec::new(),
                v: Vec::new(),
            });
        }
        let state = guard
            .as_mut()
            .ok_or_else(|| CodecError::Codec("turbojpeg encoder unavailable".into()))?;
        let mut output = OutputBuf::new_owned();
        let packed = |bpp: usize, format, subsamp| (bpp, format, subsamp);
        let packed = match &meta.format.code.to_u32().to_le_bytes() {
            b"R8  " | b"GREY" => Some(packed(1, TjPixelFormat::GRAY, TjSubsamp::Gray)),
            b"RG24" => Some(packed(3, TjPixelFormat::RGB, TjSubsamp::Sub2x2)),
            b"RGBA" => Some(packed(4, TjPixelFormat::RGBA, TjSubsamp::Sub2x2)),
            b"NV12" | b"YUYV" => None,
            _ => {
                return Err(CodecError::Codec(format!(
                    "unsupported turbojpeg encoder input {}",
                    meta.format.code
                )));
            }
        };
        if let Some((bpp, format, subsamp)) = packed {
            let pitch = plane.stride().max(width * bpp);
            let pixels = rows(plane.data(), pitch, height)?;
            state.compressor.set_subsamp(subsamp).map_err(err)?;
            let view = TjImage {
                pixels,
                width,
                pitch,
                height,
                format,
            };
            state.compressor.compress(view, &mut output).map_err(err)?;
            return finish(meta, &output);
        }
        let TurbojpegEncoderState {
            compressor,
            y,
            u,
            v,
            ..
        } = state;
        let cw = width.div_ceil(2);
        let image = if meta.format.code == FourCc::NV12 {
            let ch = height.div_ceil(2);
            let y_stride = plane.stride().max(width);
            let y_plane = rows(plane.data(), y_stride, height)?;
            // The chroma plane: the second plane, or right after the luma rows in one plane.
            let (uv, uv_stride) = match planes.get(1) {
                Some(p) => (p.data(), p.stride().max(cw * 2)),
                None => (
                    plane.data().get(y_stride * height..).unwrap_or_default(),
                    y_stride,
                ),
            };
            let uv = rows(uv, uv_stride, ch)?;
            split_pairs(uv, uv_stride, cw, ch, u, v);
            YuvPlanesImage {
                y_plane,
                u_plane: &u[..],
                v_plane: &v[..],
                width,
                height,
                y_stride,
                u_stride: cw,
                v_stride: cw,
                subsamp: TjSubsamp::Sub2x2,
            }
        } else {
            let stride = plane.stride().max(width * 2);
            let yuyv = rows(plane.data(), stride, height)?;
            split_yuyv(yuyv, stride, width, height, y, u, v);
            YuvPlanesImage {
                y_plane: &y[..],
                u_plane: &u[..],
                v_plane: &v[..],
                width,
                height,
                y_stride: width,
                u_stride: cw,
                v_stride: cw,
                subsamp: TjSubsamp::Sub2x1,
            }
        };
        compressor
            .compress_yuv_planes(&image, &mut output)
            .map_err(err)?;
        finish(meta, &output)
    }
}

/// The first `height` rows of `stride` bytes, or an error when the plane is shorter.
fn rows(data: &[u8], stride: usize, height: usize) -> Result<&[u8], CodecError> {
    let required = stride
        .checked_mul(height)
        .ok_or_else(|| CodecError::Codec("turbojpeg input stride overflow".into()))?;
    data.get(..required).ok_or_else(|| {
        CodecError::Codec("turbojpeg input frame shorter than declared stride".into())
    })
}

/// Splits interleaved pairs (NV12's UV rows) into two planes of `cw` x `ch`.
fn split_pairs(src: &[u8], stride: usize, cw: usize, ch: usize, a: &mut Vec<u8>, b: &mut Vec<u8>) {
    a.resize(cw * ch, 0);
    b.resize(cw * ch, 0);
    for row in 0..ch {
        let line = &src[row * stride..][..cw * 2];
        let (ra, rb) = (&mut a[row * cw..][..cw], &mut b[row * cw..][..cw]);
        for ((pair, x), y) in line.chunks_exact(2).zip(ra).zip(rb) {
            *x = pair[0];
            *y = pair[1];
        }
    }
}

/// Splits YUYV rows into Y (`width`), U and V (`width / 2`, rounded up) planes.
fn split_yuyv(
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    y: &mut Vec<u8>,
    u: &mut Vec<u8>,
    v: &mut Vec<u8>,
) {
    let cw = width.div_ceil(2);
    y.resize(width * height, 0);
    u.resize(cw * height, 0);
    v.resize(cw * height, 0);
    for row in 0..height {
        let line = &src[row * stride..][..width * 2];
        let ry = &mut y[row * width..][..width];
        let (ru, rv) = (&mut u[row * cw..][..cw], &mut v[row * cw..][..cw]);
        for (i, quad) in line.chunks(4).enumerate() {
            ry[2 * i] = quad[0];
            if let Some(&y1) = quad.get(2) {
                ry[2 * i + 1] = y1;
            }
            ru[i] = quad[1];
            rv[i] = quad.get(3).copied().unwrap_or(128);
        }
    }
}

impl Codec for TurbojpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.encode(&input, |meta, encoded| {
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
        })
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.encode(input, |meta, encoded| {
            shared_packet_frame(&self.descriptor, meta, encoded, pool).map(Some)
        })
    }
}

/// MJPEG decoder using libturbojpeg.
pub struct TurbojpegDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    scale: LumaDecodeScale,
}

impl TurbojpegDecoder {
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
                impl_name: "turbojpeg",
            },
            pool,
            scale: LumaDecodeScale::Full,
        }
    }

    /// Decode at ½, ¼ or ⅛ size in the DCT domain: less CPU, and a 4-64x smaller output.
    pub fn with_scale(mut self, scale: LumaDecodeScale) -> Self {
        self.scale = scale;
        self
    }

    /// Read the header and size the output: checked against the stream, then scaled.
    fn prepare(
        &self,
        tj: &TjDecompressor,
        input: &FrameLease,
        jpeg: &[u8],
    ) -> Result<(MediaFormat, PlaneLayout), CodecError> {
        let header = tj.read_header(jpeg)?;
        crate::check_decoded_size(input.meta().format.resolution, header.width, header.height)?;
        if self.scale != LumaDecodeScale::Full {
            tj.set_scaling(self.scale.denom())?;
        }
        let resolution = Resolution::new(
            self.scale.scale(header.width),
            self.scale.scale(header.height),
        )
        .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let format = MediaFormat::new(
            self.descriptor.output,
            resolution,
            input.meta().format.color,
        );
        Ok((
            format,
            plane_layout_from_dims(resolution.width, resolution.height, 3),
        ))
    }
}

impl Codec for TurbojpegDecoder {
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

        let tj = TjDecompressor::new()?;
        let (format, layout) = self.prepare(&tj, &input, plane.data())?;

        let mut buf = self.pool.lease_sized(layout.len);
        tj.decompress(
            plane.data(),
            buf.as_mut_slice(),
            layout.stride,
            turbojpeg::raw::TJPF_TJPF_RGB,
        )?;

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

        let tj = TjDecompressor::new()?;
        let (format, layout) = self.prepare(&tj, input, plane.data())?;
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(layout.len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        tj.decompress(
            plane.data(),
            lease.as_mut_slice(),
            layout.stride,
            turbojpeg::raw::TJPF_TJPF_RGB,
        )?;

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
        let fmt = MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown);
        let mut buf = BufferPool::with_limits(1, 4, 1).lease();
        buf.resize(4);
        buf.as_mut_slice().copy_from_slice(&[0, 64, 128, 255]);
        let frame = FrameLease::single_plane(FrameMeta::new(fmt, 7), buf, 4, 2);

        let encoded = TurbojpegEncoder::new(FourCc::GREY, 85)
            .process(frame)
            .expect("encode frame");
        let plane = encoded.planes().into_iter().next().expect("encoded plane");

        assert_eq!(encoded.meta().format.code, FourCc::MJPG);
        assert_eq!(encoded.meta().timestamp, 7);
        assert!(!plane.data().is_empty());
    }

    fn frame(code: FourCc, w: u32, h: u32, bytes: &[u8], stride: usize) -> FrameLease {
        let fmt = MediaFormat::new(code, Resolution::new(w, h).unwrap(), ColorSpace::Unknown);
        let mut buf = BufferPool::with_limits(1, bytes.len(), 1).lease();
        buf.resize(bytes.len());
        buf.as_mut_slice().copy_from_slice(bytes);
        FrameLease::single_plane(FrameMeta::new(fmt, 3), buf, bytes.len(), stride)
    }

    /// Encodes and decodes back to RGB: the mean colour of a uniform frame.
    fn round_trip(frame: FrameLease) -> [f64; 3] {
        let code = frame.meta().format.code;
        let jpeg = TurbojpegEncoder::new(code, 95).process(frame).unwrap();
        let rgb = TurbojpegDecoder::new(FourCc::RG24).process(jpeg).unwrap();
        let planes = rgb.planes();
        let px = planes[0].data();
        let n = (px.len() / 3) as f64;
        let mut sum = [0.0; 3];
        for p in px.chunks_exact(3) {
            for c in 0..3 {
                sum[c] += f64::from(p[c]);
            }
        }
        sum.map(|s| s / n)
    }

    // Y 81, U 90, V 240 (JPEG's full-range BT.601): RGB about (238, 14, 14).
    fn close_to_red(rgb: [f64; 3]) {
        for (got, want) in rgb.iter().zip([238.0, 14.0, 14.0]) {
            assert!((got - want).abs() < 6.0, "{rgb:?}");
        }
    }

    #[test]
    fn turbojpeg_encoder_encodes_nv12_from_its_planes() {
        let (w, h) = (16, 10);
        let mut bytes = vec![81u8; w * h];
        for _ in 0..(w * h / 4) {
            bytes.extend_from_slice(&[90, 240]);
        }
        close_to_red(round_trip(frame(FourCc::NV12, 16, 10, &bytes, w)));
    }

    #[test]
    fn turbojpeg_encoder_encodes_yuyv_with_padded_rows() {
        let (w, h, stride) = (16, 10, 40);
        let mut bytes = Vec::new();
        for _ in 0..h {
            for _ in 0..w / 2 {
                bytes.extend_from_slice(&[81, 90, 81, 240]);
            }
            bytes.extend_from_slice(&[0; 8]);
        }
        close_to_red(round_trip(frame(FourCc::YUYV, 16, 10, &bytes, stride)));
        let short = frame(FourCc::YUYV, 16, 10, &bytes[..stride * 9], stride);
        assert!(
            TurbojpegEncoder::new(FourCc::YUYV, 90)
                .process(short)
                .is_err()
        );
    }
}

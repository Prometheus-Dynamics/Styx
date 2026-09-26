//! MJPEG → GREY (Y8) decoding with libturbojpeg's luma-only path.
//!
//! Requesting grayscale output from a colour JPEG makes libjpeg-turbo skip the IDCT, upsampling
//! and colour conversion of both chroma components; only the entropy decode still covers them.
//! On a Cortex-A76 this decodes 1280x800 camera frames in ~2.4 ms versus ~4.5 ms for RGB with
//! the same library and ~20 ms for `jpeg-decoder` RGB.

use std::ffi::c_int;

use smallvec::smallvec;
use styx_core::prelude::*;
use turbojpeg::raw;

use crate::turbojpeg_raw::{JpegHeader, TjDecompressor};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_SPARE};

/// DCT-domain downscale applied while decoding.
///
/// Scaling skips IDCT work but not entropy decoding, so ½ saves ~15% and ⅛ ~40% on a CM5; a
/// full decode plus a 2×2 box filter is usually the better way to get a pyramid level.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LumaDecodeScale {
    #[default]
    Full,
    Half,
    Quarter,
    Eighth,
}

impl LumaDecodeScale {
    fn denom(self) -> c_int {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
            Self::Eighth => 8,
        }
    }

    /// Output size of a `dimension`-pixel edge at this scale (rounded up, as libjpeg-turbo does).
    pub fn scale(self, dimension: u32) -> u32 {
        dimension.div_ceil(self.denom() as u32)
    }
}

/// Region of the (scaled) image to decode.
///
/// libjpeg-turbo requires `x` to be a multiple of the scaled iMCU width, so the decoder moves `x`
/// down to that boundary and widens the region to keep the requested pixels. Use
/// [`TurbojpegLumaDecoder::effective_crop`] to learn the region actually decoded. Rows below the
/// region are skipped entirely; rows above it must still be entropy-decoded, so crops near the
/// top of the image save the most.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LumaCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Options for [`TurbojpegLumaDecoder`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LumaDecodeOptions {
    /// Row stride and buffer base alignment in bytes (power of two). Default 64.
    pub stride_alignment: usize,
    pub scale: LumaDecodeScale,
    pub crop: Option<LumaCrop>,
    /// Use libjpeg-turbo's faster, slightly less accurate integer IDCT (~7-15% faster on ARM).
    pub fast_dct: bool,
    /// Attach GREY pyramid companions `1..=pyramid_levels` (½, ¼, ...) computed with 2×2 box
    /// filters from the decoded frame; read them with `FrameLease::pyramid_level`. One full
    /// decode plus box filters is far cheaper than a second, DCT-scaled decode.
    pub pyramid_levels: u8,
    /// Decode one frame on up to this many threads by splitting it at restart markers
    /// (`0` = automatic, the default: up to 4 of the available cores; `1` = single-threaded).
    /// Output is byte-identical either way. Only full-resolution,
    /// uncropped decodes of baseline JPEGs with restart markers are split; others decode on one
    /// thread. Many UVC cameras emit restart markers on every frame.
    pub threads: usize,
}

impl Default for LumaDecodeOptions {
    fn default() -> Self {
        Self {
            stride_alignment: 64,
            scale: LumaDecodeScale::Full,
            crop: None,
            fast_dct: false,
            pyramid_levels: 0,
            threads: 0,
        }
    }
}

/// MJPEG → GREY decoder using libturbojpeg, writing 64-byte-aligned rows into pooled buffers.
pub struct TurbojpegLumaDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    /// Recycled buffers for box-filter pyramid levels.
    pyramid_pool: BufferPool,
    options: LumaDecodeOptions,
}

impl TurbojpegLumaDecoder {
    pub fn new() -> Self {
        Self::with_options(LumaDecodeOptions::default())
    }

    pub fn with_options(options: LumaDecodeOptions) -> Self {
        Self::with_pool(
            options,
            BufferPool::lazy(1280 * 800 + 64, DEFAULT_CODEC_POOL_SPARE),
        )
    }

    pub fn with_pool(mut options: LumaDecodeOptions, pool: BufferPool) -> Self {
        if !options.stride_alignment.is_power_of_two() {
            options.stride_alignment = 1;
        }
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::MJPG,
                output: FourCc::GREY,
                name: "mjpeg",
                impl_name: "turbojpeg-luma",
            },
            pool,
            pyramid_pool: BufferPool::lazy(0, DEFAULT_CODEC_POOL_SPARE * 2),
            options,
        }
    }

    pub fn options(&self) -> LumaDecodeOptions {
        self.options
    }

    /// Region decoded for a JPEG with the given header, after scaling and iMCU alignment.
    pub fn effective_crop(&self, jpeg: &[u8]) -> Result<Option<LumaCrop>, CodecError> {
        let handle = TjDecompressor::new()?;
        let header = handle.read_header(jpeg)?;
        Ok(self.plan(&header).crop)
    }

    fn plan(&self, header: &JpegHeader) -> DecodePlan {
        let scale = self.options.scale;
        let scaled_w = scale.scale(header.width);
        let scaled_h = scale.scale(header.height);
        let crop = self.options.crop.and_then(|crop| {
            let mcu = scale.scale(header.mcu_width).max(1);
            let x = crop.x.min(scaled_w.saturating_sub(1));
            let y = crop.y.min(scaled_h.saturating_sub(1));
            let aligned_x = x - x % mcu;
            let right = (x.saturating_add(crop.width)).min(scaled_w);
            let bottom = (y.saturating_add(crop.height)).min(scaled_h);
            let region = LumaCrop {
                x: aligned_x,
                y,
                width: right.saturating_sub(aligned_x),
                height: bottom.saturating_sub(y),
            };
            (region.width > 0 && region.height > 0).then_some(region)
        });
        let (width, height) = crop.map_or((scaled_w, scaled_h), |c| (c.width, c.height));
        let stride = (width as usize).next_multiple_of(self.options.stride_alignment);
        DecodePlan {
            width,
            height,
            stride,
            crop,
        }
    }

    fn decode_into(
        &self,
        handle: &TjDecompressor,
        jpeg: &[u8],
        plan: &DecodePlan,
        dst: &mut [u8],
    ) -> Result<(), CodecError> {
        if let Some(()) = self.decode_parallel(jpeg, plan, dst)? {
            return Ok(());
        }
        handle.set(raw::TJPARAM_TJPARAM_FASTDCT, self.options.fast_dct as c_int)?;
        handle.set_scaling(self.options.scale.denom())?;
        handle.set_crop(plan.crop)?;
        handle.decompress(jpeg, dst, plan.stride, raw::TJPF_TJPF_GRAY)
    }
}

impl TurbojpegLumaDecoder {
    fn thread_count(&self) -> usize {
        match self.options.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| usize::from(n).min(4)),
            n => n,
        }
    }

    /// Decode restart-marker slices on separate threads straight into their rows of `dst`.
    /// `Ok(None)` means the frame is not splittable and the caller should decode it whole.
    fn decode_parallel(
        &self,
        jpeg: &[u8],
        plan: &DecodePlan,
        dst: &mut [u8],
    ) -> Result<Option<()>, CodecError> {
        let threads = self.thread_count();
        if threads < 2 || plan.crop.is_some() || self.options.scale != LumaDecodeScale::Full {
            return Ok(None);
        }
        let Some(slices) = crate::jpeg_slices::split_jpeg(jpeg, threads) else {
            return Ok(None);
        };
        let stride = plan.stride;
        let fast_dct = self.options.fast_dct;
        let total = dst.len();
        let mut rest = &mut dst[..];
        let mut work = Vec::with_capacity(slices.len());
        for slice in &slices {
            debug_assert_eq!(slice.first_row * stride, total - rest.len());
            let (rows, tail) = std::mem::take(&mut rest).split_at_mut(slice.rows * stride);
            rest = tail;
            work.push((slice, rows));
        }
        let results: Vec<Result<(), CodecError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = work
                .into_iter()
                .map(|(slice, rows)| {
                    scope.spawn(move || {
                        let handle = TjDecompressor::new()?;
                        handle.set(raw::TJPARAM_TJPARAM_FASTDCT, fast_dct as c_int)?;
                        handle.read_header(&slice.jpeg)?;
                        handle.decompress(&slice.jpeg, rows, stride, raw::TJPF_TJPF_GRAY)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(CodecError::Codec("decode thread panicked".into())))
                })
                .collect()
        });
        if results.iter().any(Result::is_err) {
            // Fall back to a whole-frame decode rather than dropping the frame.
            return Ok(None);
        }
        Ok(Some(()))
    }

    fn attach_pyramid(&self, frame: FrameLease) -> Result<FrameLease, CodecError> {
        if self.options.pyramid_levels == 0 {
            return Ok(frame);
        }
        frame
            .with_box_pyramid_in(
                self.options.pyramid_levels,
                self.options.stride_alignment,
                &self.pyramid_pool,
            )
            .map_err(|err| CodecError::Codec(err.to_string()))
    }
}

impl Default for TurbojpegLumaDecoder {
    fn default() -> Self {
        Self::new()
    }
}

struct DecodePlan {
    width: u32,
    height: u32,
    stride: usize,
    crop: Option<LumaCrop>,
}

impl Codec for TurbojpegLumaDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let jpeg = mjpeg_payload(&input, &self.descriptor)?;
        let handle = TjDecompressor::new()?;
        let header = handle.read_header(jpeg)?;
        let plan = self.plan(&header);
        let resolution = Resolution::new(plan.width, plan.height)
            .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let len = plan.stride * plan.height as usize;
        let align = self.options.stride_alignment;

        let mut buf = self.pool.lease();
        buf.resize(len + align - 1);
        let base = buf.as_slice().as_ptr() as usize;
        let offset = base.next_multiple_of(align) - base;
        self.decode_into(
            &handle,
            jpeg,
            &plan,
            &mut buf.as_mut_slice()[offset..offset + len],
        )?;

        let format = MediaFormat::new(FourCc::GREY, resolution, input.meta().format.color);
        let meta = luma_meta(input.meta(), format);
        let frame = FrameLease::multi_plane(
            meta,
            smallvec![buf],
            smallvec![PlaneLayout {
                offset,
                len,
                stride: plan.stride,
            }],
        );
        self.attach_pyramid(frame)
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        let jpeg = mjpeg_payload(input, &self.descriptor)?;
        let handle = TjDecompressor::new()?;
        let header = handle.read_header(jpeg)?;
        let plan = self.plan(&header);
        let resolution = Resolution::new(plan.width, plan.height)
            .ok_or_else(|| CodecError::Codec("invalid jpeg resolution".into()))?;
        let len = plan.stride * plan.height as usize;
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        // Shared leases are page-aligned mmaps, so the base already satisfies the alignment.
        self.decode_into(&handle, jpeg, &plan, &mut lease.as_mut_slice()[..len])?;
        let format = MediaFormat::new(FourCc::GREY, resolution, input.meta().format.color);
        let frame = FrameLease::single_plane_shared(
            luma_meta(input.meta(), format),
            lease,
            len,
            plan.stride,
        )
        .map_err(|err| CodecError::Codec(err.to_string()))?;
        self.attach_pyramid(frame).map(Some)
    }
}

fn mjpeg_payload<'a>(
    input: &'a FrameLease,
    descriptor: &CodecDescriptor,
) -> Result<&'a [u8], CodecError> {
    let code = input.meta().format.code;
    if !code.is_jpeg_encoded() {
        return Err(CodecError::FormatMismatch {
            expected: descriptor.input,
            actual: code,
        });
    }
    let plane = input
        .planes()
        .into_iter()
        .next()
        .ok_or_else(|| CodecError::Codec("mjpeg frame missing plane".into()))?;
    Ok(plane.data())
}

fn luma_meta(input: &FrameMeta, format: MediaFormat) -> FrameMeta {
    let mut meta = FrameMeta::new(format, input.timestamp);
    meta.backend = input.backend.clone();
    meta.capture_instant = input.capture_instant;
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mjpeg_turbojpeg::TurbojpegEncoder;

    fn gradient_jpeg(width: u32, height: u32) -> FrameLease {
        let res = Resolution::new(width, height).unwrap();
        let stride = width as usize * 3;
        let mut buf = BufferPool::with_limits(1, stride * height as usize, 1).lease();
        buf.resize(stride * height as usize);
        for (i, px) in buf.as_mut_slice().chunks_exact_mut(3).enumerate() {
            let x = (i % width as usize) as u8;
            px.copy_from_slice(&[x, x, x]);
        }
        let rgb = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb), 99),
            buf,
            stride * height as usize,
            stride,
        );
        TurbojpegEncoder::new(FourCc::RG24, 95)
            .process(rgb)
            .expect("encode")
    }

    #[test]
    fn decodes_to_aligned_grey_with_timestamp() {
        let decoded = TurbojpegLumaDecoder::new()
            .process(gradient_jpeg(100, 40))
            .expect("decode");
        assert_eq!(decoded.meta().format.code, FourCc::GREY);
        assert_eq!(decoded.meta().timestamp, 99);
        assert_eq!(decoded.plane_strides().as_slice(), &[128]);
        let rows = decoded.luma_rows().expect("rows");
        assert_eq!(rows.row_bytes(), 100);
        assert_eq!(decoded.planes()[0].data().as_ptr() as usize % 64, 0);
        let row = rows.row(20).unwrap().data();
        assert!(row[10].abs_diff(10) <= 3 && row[90].abs_diff(90) <= 3);
    }

    #[test]
    fn scaled_and_cropped_decodes_report_their_geometry() {
        let jpeg = gradient_jpeg(128, 64);
        let half = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            scale: LumaDecodeScale::Half,
            ..Default::default()
        });
        let out = half.process(jpeg).expect("half");
        assert_eq!(out.meta().format.resolution.width.get(), 64);
        assert_eq!(out.meta().format.resolution.height.get(), 32);

        let pyramid = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            pyramid_levels: 2,
            ..Default::default()
        })
        .process(gradient_jpeg(128, 64))
        .expect("pyramid");
        let quarter = pyramid.pyramid_level(2).expect("quarter level");
        assert_eq!(quarter.meta().format.resolution.width.get(), 32);
        assert_eq!(quarter.meta().timestamp, pyramid.meta().timestamp);

        let crop = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            crop: Some(LumaCrop {
                x: 20,
                y: 8,
                width: 40,
                height: 16,
            }),
            ..Default::default()
        });
        let jpeg = gradient_jpeg(128, 64);
        let region = crop
            .effective_crop(jpeg.planes()[0].data())
            .unwrap()
            .unwrap();
        assert_eq!(region.x % 8, 0);
        assert!(region.x <= 20 && region.x + region.width >= 60);
        let out = crop.process(jpeg).expect("crop");
        assert_eq!(out.meta().format.resolution.width.get(), region.width);
        assert_eq!(out.meta().format.resolution.height.get(), 16);
        let first = out.luma_rows().unwrap().row(0).unwrap().data()[0];
        assert!(first.abs_diff(region.x as u8) <= 3);
    }

    #[test]
    fn uvc_style_corrupt_padding_is_a_warning_not_a_dropped_frame() {
        // Many UVC cameras emit junk bytes before a marker; libjpeg decodes the full image and
        // reports "extraneous bytes before marker" as a warning.
        let clean = gradient_jpeg(64, 32);
        let mut bytes = clean.planes()[0].data().to_vec();
        let eoi = bytes.len() - 2;
        assert_eq!(&bytes[eoi..], &[0xff, 0xd9]);
        bytes.splice(eoi..eoi, [0x12u8; 7]);
        let res = Resolution::new(64, 32).unwrap();
        let frame = || {
            let mut buf = BufferPool::with_limits(1, bytes.len(), 1).lease();
            buf.resize(bytes.len());
            buf.as_mut_slice().copy_from_slice(&bytes);
            FrameLease::single_plane(
                FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 5),
                buf,
                bytes.len(),
                bytes.len(),
            )
        };
        let grey = TurbojpegLumaDecoder::new()
            .process(frame())
            .expect("luma decode");
        assert_eq!(grey.meta().format.resolution.width.get(), 64);
        let rgb = crate::mjpeg_turbojpeg::TurbojpegDecoder::new(FourCc::RG24)
            .process(frame())
            .expect("rgb decode");
        assert_eq!(rgb.meta().format.code, FourCc::RG24);
    }

    fn restart_jpeg(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; width * height * 3];
        for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
            let (x, y) = (i % width, i / width);
            px.copy_from_slice(&[(x * 7 + y) as u8, (x ^ y) as u8, (y * 3) as u8]);
        }
        unsafe {
            let h = raw::tj3Init(raw::TJINIT_TJINIT_COMPRESS as c_int);
            raw::tj3Set(h, raw::TJPARAM_TJPARAM_QUALITY as c_int, 90);
            raw::tj3Set(
                h,
                raw::TJPARAM_TJPARAM_SUBSAMP as c_int,
                raw::TJSAMP_TJSAMP_422 as c_int,
            );
            raw::tj3Set(h, raw::TJPARAM_TJPARAM_RESTARTROWS as c_int, 1);
            let mut out: *mut u8 = std::ptr::null_mut();
            let mut size = 0;
            let ret = raw::tj3Compress8(
                h,
                rgb.as_ptr(),
                width as c_int,
                (width * 3) as c_int,
                height as c_int,
                raw::TJPF_TJPF_RGB as c_int,
                &mut out,
                &mut size,
            );
            assert_eq!(ret, 0);
            let jpeg = std::slice::from_raw_parts(out, size as usize).to_vec();
            raw::tj3Free(out.cast());
            raw::tj3Destroy(h);
            jpeg
        }
    }

    #[test]
    fn parallel_restart_slices_match_single_threaded_decode() {
        let jpeg = restart_jpeg(200, 67);
        assert!(crate::jpeg_slices::split_jpeg(&jpeg, 4).is_some_and(|s| s.len() > 1));
        let frame = || {
            let res = Resolution::new(200, 67).unwrap();
            let mut buf = BufferPool::with_limits(1, jpeg.len(), 1).lease();
            buf.resize(jpeg.len());
            buf.as_mut_slice().copy_from_slice(&jpeg);
            FrameLease::single_plane(
                FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 1),
                buf,
                jpeg.len(),
                jpeg.len(),
            )
        };
        let rows = |f: &FrameLease| -> Vec<Vec<u8>> {
            f.luma_rows()
                .unwrap()
                .iter()
                .map(|r| r.data().to_vec())
                .collect()
        };
        let single = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            threads: 1,
            ..Default::default()
        })
        .process(frame())
        .unwrap();
        for threads in [2, 3, 4, 0] {
            let parallel = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
                threads,
                ..Default::default()
            })
            .process(frame())
            .unwrap();
            assert_eq!(rows(&single), rows(&parallel), "threads={threads}");
        }
    }

    #[test]
    fn registry_resolves_mjpeg_to_grey() {
        let registry = crate::CodecRegistry::with_enabled_codecs().expect("registry");
        let codec = registry
            .handle()
            .lookup_for_output(FourCc::MJPG, FourCc::GREY)
            .expect("MJPG -> GREY decoder");
        assert_eq!(codec.descriptor().output, FourCc::GREY);
    }
}

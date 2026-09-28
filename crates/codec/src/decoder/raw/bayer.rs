//! Raw Bayer frames through the software ISP (`styx-softisp`) to RGB24, NV12 or luma.
//!
//! One decoder per output format and Bayer input format, so the planner finds the cheapest
//! route (`GREY` straight from the mosaic, `NV12` without an RGB intermediate). Parameters
//! (black level, white balance, colour matrix, tone curve, ...) default to a plain linear
//! pipeline and can be changed at run time with [`SoftIspDecoder::set_params`]; when they ask
//! for statistics, the last frame's are kept for AE / AWB ([`SoftIspDecoder::last_stats`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use smallvec::smallvec;
use styx_core::prelude::*;
use styx_softisp::{IspParams, IspStats, OutputBuffers, RawFormat, Scale, SoftIsp};

use crate::{Codec, CodecDescriptor, CodecError};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};

/// Frames at least this large are split into row bands over the rayon pool.
const PARALLEL_MIN_PIXELS: usize = 640 * 480;

/// The output a [`SoftIspDecoder`] produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftIspOutput {
    Rgb24,
    Nv12,
    Luma,
}

impl SoftIspOutput {
    fn fourcc(self) -> FourCc {
        match self {
            Self::Rgb24 => FourCc::RG24,
            Self::Nv12 => FourCc::NV12,
            Self::Luma => FourCc::GREY,
        }
    }
}

/// Raw Bayer (any V4L2 Bayer format, packed or not) to RGB24, NV12 or luma on the CPU.
pub struct SoftIspDecoder {
    descriptor: CodecDescriptor,
    output: SoftIspOutput,
    params: RwLock<(u64, IspParams)>,
    generation: AtomicU64,
    /// Idle ISPs, each with its parameter generation.
    isps: Mutex<Vec<(u64, SoftIsp)>>,
    last_stats: Mutex<Option<IspStats>>,
    pool: BufferPool,
    uv_pool: BufferPool,
}

impl SoftIspDecoder {
    /// `None` when `input` is not a Bayer format.
    pub fn new(
        input: FourCc,
        output: SoftIspOutput,
        max_width: u32,
        max_height: u32,
    ) -> Option<Self> {
        styx_softisp::bayer_fourcc(input)?;
        let px = max_width as usize * max_height as usize;
        let (name, bytes) = match output {
            SoftIspOutput::Rgb24 => ("bayer2rgb", px * 3),
            SoftIspOutput::Nv12 => ("bayer2nv12", px),
            SoftIspOutput::Luma => ("bayer2grey", px),
        };
        Some(Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                output.fourcc(),
                name,
                "softisp",
            ),
            output,
            params: RwLock::new((0, IspParams::default())),
            generation: AtomicU64::new(0),
            isps: Mutex::new(Vec::new()),
            last_stats: Mutex::new(None),
            pool: BufferPool::lazy(bytes, 4),
            uv_pool: BufferPool::lazy(px / 2, 4),
        })
    }

    /// Use these parameters from the next frame on.
    pub fn set_params(&self, params: IspParams) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        *self.params.write() = (generation, params);
    }

    pub fn params(&self) -> IspParams {
        self.params.read().1.clone()
    }

    /// Statistics of the last frame, when the parameters ask for them.
    pub fn last_stats(&self) -> Option<IspStats> {
        self.last_stats.lock().clone()
    }

    /// Run the ISP on `input` into `out` (sized for the input resolution).
    fn run(&self, input: &FrameLease, out: OutputBuffers<'_>) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let res = meta.format.resolution;
        let format = RawFormat::from_fourcc(meta.format.code, res.width.get(), res.height.get())
            .ok_or_else(|| CodecError::Codec("not a Bayer format".into()))?;
        let planes = input.planes();
        let plane = planes
            .first()
            .ok_or_else(|| CodecError::Codec("bayer frame missing plane".into()))?;
        let stride = plane.stride().max(format.min_stride());

        let mut isp = self.isp(format)?;
        let result = isp.1.process(plane.data(), stride, Scale::Full, out);
        let yuv = isp.1.params().yuv;
        self.isps.lock().push(isp);
        let stats = result.map_err(|e| CodecError::Codec(e.to_string()))?;
        if stats.is_some() {
            *self.last_stats.lock() = stats;
        }
        let color = match self.output {
            SoftIspOutput::Rgb24 => ColorSpace::Srgb,
            SoftIspOutput::Nv12 => match yuv {
                styx_softisp::YuvMatrix::Bt709Limited => ColorSpace::Bt709,
                styx_softisp::YuvMatrix::Bt601Full => ColorSpace::Srgb,
            },
            SoftIspOutput::Luma => meta.format.color,
        };
        Ok(FrameMeta::new(
            MediaFormat::new(self.descriptor.output, res, color),
            meta.timestamp,
        ))
    }

    /// An idle ISP for `format` with the current parameters.
    fn isp(&self, format: RawFormat) -> Result<(u64, SoftIsp), CodecError> {
        let (generation, params) = {
            let p = self.params.read();
            (p.0, p.1.clone())
        };
        let cached = {
            let mut isps = self.isps.lock();
            isps.iter()
                .position(|(_, isp)| isp.format() == format)
                .map(|i| isps.swap_remove(i))
        };
        let threads = if format.width as usize * format.height as usize >= PARALLEL_MIN_PIXELS {
            0
        } else {
            1
        };
        let err = |e: styx_softisp::IspError| CodecError::Codec(e.to_string());
        match cached {
            Some((g, isp)) if g == generation => Ok((g, isp)),
            Some((_, mut isp)) => {
                isp.set_params(params).map_err(err)?;
                Ok((generation, isp))
            }
            None => Ok((
                generation,
                SoftIsp::new(format, params)
                    .map_err(err)?
                    .with_threads(threads),
            )),
        }
    }

    /// The output into `dst`: RGB24 or GREY, or NV12 with the UV plane right after Y.
    pub fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        let width = input.meta().format.resolution.width.get() as usize;
        let out = match self.output {
            SoftIspOutput::Rgb24 => OutputBuffers::Rgb24 {
                data: dst,
                stride: width * 3,
            },
            SoftIspOutput::Luma => OutputBuffers::Luma {
                data: dst,
                stride: width,
            },
            SoftIspOutput::Nv12 => {
                let height = input.meta().format.resolution.height.get() as usize;
                let (y, uv) = dst.split_at_mut((width * height).min(dst.len()));
                OutputBuffers::Nv12 {
                    y,
                    y_stride: width,
                    uv,
                    uv_stride: width,
                }
            }
        };
        self.run(input, out)
    }

    fn nv12_layouts(res: Resolution) -> [PlaneLayout; 2] {
        let (w, h) = (res.width.get() as usize, res.height.get() as usize);
        [
            PlaneLayout {
                offset: 0,
                len: w * h,
                stride: w,
            },
            PlaneLayout {
                offset: 0,
                len: w * h.div_ceil(2),
                stride: w,
            },
        ]
    }
}

impl Codec for SoftIspDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        match self.output {
            SoftIspOutput::Rgb24 | SoftIspOutput::Luma => {
                let bpp = if self.output == SoftIspOutput::Rgb24 {
                    3
                } else {
                    1
                };
                crate::decoder::raw::process_owned_raw_decode(input, &self.pool, bpp, |i, d| {
                    self.decode_into(i, d)
                })
            }
            SoftIspOutput::Nv12 => {
                let [yl, uvl] = Self::nv12_layouts(input.meta().format.resolution);
                let (mut y, mut uv) = (self.pool.lease(), self.uv_pool.lease());
                y.resize(yl.len);
                uv.resize(uvl.len);
                let out = OutputBuffers::Nv12 {
                    y: y.as_mut_slice(),
                    y_stride: yl.stride,
                    uv: uv.as_mut_slice(),
                    uv_stride: uvl.stride,
                };
                let meta = self.run(&input, out)?;
                Ok(FrameLease::multi_plane(
                    meta,
                    smallvec![y, uv],
                    smallvec![yl, uvl],
                ))
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        if self.output != SoftIspOutput::Nv12 {
            return crate::decoder::raw::process_shared_raw_decode(self, input, pool);
        }
        let [yl, mut uvl] = Self::nv12_layouts(input.meta().format.resolution);
        uvl.offset = yl.len;
        let map = |e: FrameExportError| CodecError::Codec(e.to_string());
        let mut lease = pool.lease().map_err(map)?;
        lease.try_resize(yl.len + uvl.len).map_err(map)?;
        let (y, uv) = lease.as_mut_slice().split_at_mut(yl.len);
        let out = OutputBuffers::Nv12 {
            y,
            y_stride: yl.stride,
            uv,
            uv_stride: uvl.stride,
        };
        let meta = self.run(input, out)?;
        FrameLease::multi_plane_shared(meta, lease, smallvec![yl, uvl])
            .map(Some)
            .map_err(map)
    }
}

#[cfg(target_os = "linux")]
impl crate::decoder::raw::RawDecodeInto for SoftIspDecoder {
    fn output_bytes_per_pixel(&self) -> usize {
        match self.output {
            SoftIspOutput::Rgb24 => 3,
            SoftIspOutput::Luma | SoftIspOutput::Nv12 => 1,
        }
    }

    fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        SoftIspDecoder::decode_into(self, input, dst)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for SoftIspDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

/// The software ISP decoders for Bayer format `fourcc` (RGB24, NV12, GREY), empty for other
/// formats.
pub fn bayer_decoders_for(fourcc: FourCc, max_width: u32, max_height: u32) -> Vec<Arc<dyn Codec>> {
    [
        SoftIspOutput::Rgb24,
        SoftIspOutput::Nv12,
        SoftIspOutput::Luma,
    ]
    .into_iter()
    .filter_map(|out| SoftIspDecoder::new(fourcc, out, max_width, max_height))
    .map(|d| Arc::new(d) as Arc<dyn Codec>)
    .collect()
}

#[cfg(test)]
#[path = "bayer_tests.rs"]
mod tests;

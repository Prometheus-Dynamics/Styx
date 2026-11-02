//! [`GpuStyxH264`]: [`StyxH264`] with the motion search of each P picture started from the
//! GPU's exhaustive whole-pel search (`styx_gpuisp::GpuConvert::motion_search`, the result of
//! `styx_core::simd::h264::full_search` for every macroblock); the Lagrangian refinement,
//! macroblock decisions and coding stay on the CPU.

use styx_gpuisp::{GpuContext, GpuConvert, MotionCandidate};

use super::{StyxH264, StyxH264Input};
use crate::CodecError;

/// The GPU's search range: whole pels either way.
pub const GPU_SEARCH_RANGE: i32 = 16;

/// The encoder, the GPU's search and its results, kept between pictures.
pub struct GpuStyxH264 {
    convert: GpuConvert,
    h264: StyxH264,
    found: Vec<MotionCandidate>,
}

impl std::fmt::Debug for GpuStyxH264 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuStyxH264")
            .field("h264", &self.h264)
            .finish()
    }
}

fn gpu_err(e: styx_gpuisp::GpuError) -> CodecError {
    CodecError::Codec(format!("gpu h264: {e}"))
}

impl GpuStyxH264 {
    /// An encoder at `qp` with an IDR picture every `keyframe_interval`, searching on `ctx`'s
    /// device.
    pub fn new(ctx: &GpuContext, qp: u8, keyframe_interval: u32) -> Result<Self, CodecError> {
        Ok(Self {
            convert: GpuConvert::new(ctx).map_err(gpu_err)?,
            h264: StyxH264::new(qp, keyframe_interval),
            found: Vec::new(),
        })
    }

    /// The CPU encoder underneath.
    pub fn encoder(&mut self) -> &mut StyxH264 {
        &mut self.h264
    }

    /// [`StyxH264::encode`] with the GPU's search.
    pub fn encode(
        &mut self,
        input: &StyxH264Input<'_>,
        out: &mut Vec<u8>,
    ) -> Result<bool, CodecError> {
        let (convert, found) = (&mut self.convert, &mut self.found);
        self.h264
            .encode_hinted(input, out, |cur, reference, size, hints| {
                convert
                    .motion_search(cur, reference, size, GPU_SEARCH_RANGE, found)
                    .map_err(gpu_err)?;
                hints.clear();
                hints.extend(found.iter().map(|c| c.mv));
                Ok(())
            })
    }
}

#[cfg(test)]
mod tests {
    use styx_gpuisp::DeviceSelect;

    use super::*;
    use crate::styx_h264::tests::{ffmpeg_decode, plane_rows, scene};

    #[test]
    fn gpu_search_decodes_and_finds_fast_motion() {
        let (w, h) = (176, 144);
        for d in styx_gpuisp::devices() {
            let Ok(ctx) = GpuContext::open(DeviceSelect::Index(d.index)) else {
                continue;
            };
            let mut gpu = GpuStyxH264::new(&ctx, 26, 30).unwrap();
            let mut cpu = StyxH264::new(26, 30);
            let (mut stream, mut recon) = (Vec::new(), Vec::new());
            let (mut out, mut gpu_bytes, mut cpu_bytes) = (Vec::new(), 0, 0);
            for f in 0..6 {
                // 13 pixels a picture: beyond what the diamond reaches from zero alone.
                let (y, u, v) = scene(w, h, f * 13);
                let input = StyxH264Input::I420 {
                    y: &y,
                    u: &u,
                    v: &v,
                    width: w,
                    height: h,
                    y_stride: w,
                    c_stride: w / 2,
                };
                gpu.encode(&input, &mut out).unwrap();
                stream.extend_from_slice(&out);
                gpu_bytes += out.len();
                let r = &gpu.h264.reference;
                recon.extend(plane_rows(&r.y, (w, h)));
                recon.extend(plane_rows(&r.u, (w / 2, h / 2)));
                recon.extend(plane_rows(&r.v, (w / 2, h / 2)));
                cpu.encode(&input, &mut out).unwrap();
                cpu_bytes += out.len();
            }
            assert!(
                gpu_bytes <= cpu_bytes,
                "{}: gpu {gpu_bytes} cpu {cpu_bytes}",
                d.name
            );
            if let Some(decoded) = ffmpeg_decode(&stream) {
                assert!(decoded == recon, "{}: the decode differs", d.name);
            }
        }
    }
}

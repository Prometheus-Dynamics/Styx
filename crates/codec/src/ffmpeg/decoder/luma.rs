//! Hardware-accelerated MJPEG/H.264/H.265 → GREY decoding through FFmpeg.
//!
//! Covers SoC decoders exposed as named FFmpeg decoders (Rockchip MPP via ffmpeg-rockchip,
//! V4L2 M2M, NVIDIA Jetson builds) whose DRM-PRIME output is mapped zero-copy and viewed as Y8,
//! and desktop/mini-PC devices (VA-API, CUDA/NVDEC, QSV) whose surfaces are transferred to
//! system memory first. Use [`FfmpegLumaDecoder::best_available`] to pick whatever the machine
//! supports and fall back to `TurbojpegLumaDecoder` when it returns `None`.

use crate::ffmpeg::ff::codec::Id;
use styx_core::prelude::*;

use super::{FfmpegHwDevice, FfmpegVideoDecoder};
use crate::{Codec, CodecDescriptor, CodecError};

/// Named SoC decoders, most specific first. Only those present in the linked FFmpeg build and
/// able to open on this machine are used.
/// Hardware device types present on this machine.
fn hardware_devices() -> Vec<FfmpegHwDevice> {
    use crate::ffmpeg::hw_presence::{cuda, qsv, vaapi};
    [
        (FfmpegHwDevice::Vaapi, vaapi()),
        (FfmpegHwDevice::Cuda, cuda()),
        (FfmpegHwDevice::Qsv, qsv()),
    ]
    .into_iter()
    .filter_map(|(device, present)| present.then_some(device))
    .collect()
}

fn named_candidates(input: FourCc) -> &'static [&'static str] {
    match &input.to_u32().to_le_bytes() {
        b"MJPG" | b"JPEG" => &[
            "mjpeg_rkmpp",
            "mjpeg_v4l2m2m",
            "mjpeg_nvv4l2dec",
            "mjpeg_qsv",
            "mjpeg_cuvid",
        ],
        b"H264" => &[
            "h264_rkmpp",
            "h264_v4l2m2m",
            "h264_nvv4l2dec",
            "h264_nvmpi",
            "h264_qsv",
            "h264_cuvid",
        ],
        b"H265" | b"HEVC" => &[
            "hevc_rkmpp",
            "hevc_v4l2m2m",
            "hevc_nvv4l2dec",
            "hevc_nvmpi",
            "hevc_qsv",
            "hevc_cuvid",
        ],
        _ => &[],
    }
}

fn codec_id(input: FourCc) -> Result<(Id, &'static str), CodecError> {
    match &input.to_u32().to_le_bytes() {
        b"MJPG" | b"JPEG" => Ok((Id::MJPEG, "mjpeg")),
        b"H264" => Ok((Id::H264, "h264")),
        b"H265" | b"HEVC" => Ok((Id::HEVC, "h265")),
        _ => Err(CodecError::Codec(format!(
            "no ffmpeg hardware luma decoder for {input}"
        ))),
    }
}

/// FFmpeg decoder producing GREY frames, optionally on hardware.
pub struct FfmpegLumaDecoder {
    inner: FfmpegVideoDecoder,
    backend: String,
}

impl FfmpegLumaDecoder {
    /// FFmpeg's native decoder for `input`, on `device` when given (VA-API, CUDA, QSV, DRM).
    pub fn new(input: FourCc, device: Option<FfmpegHwDevice>) -> Result<Self, CodecError> {
        let (id, name) = codec_id(input)?;
        let inner = FfmpegVideoDecoder::new(
            id,
            name,
            "ffmpeg-hw",
            input,
            FourCc::GREY,
            false,
            None,
            None,
            true,
            id == Id::MJPEG,
        )?;
        let (inner, backend) = match device {
            Some(device) => (inner.with_hw_device(device)?, format!("{name}+{device:?}")),
            None => (inner, name.to_string()),
        };
        Ok(Self { inner, backend })
    }

    /// A named FFmpeg decoder such as `mjpeg_rkmpp`, `h264_v4l2m2m` or `h264_nvv4l2dec`.
    /// DRM-PRIME output is mapped zero-copy and exposed as a Y8 view.
    pub fn by_name(input: FourCc, decoder_name: &'static str) -> Result<Self, CodecError> {
        let (id, name) = codec_id(input)?;
        let inner = FfmpegVideoDecoder::new_by_name(
            decoder_name,
            name,
            "ffmpeg-hw",
            input,
            FourCc::GREY,
            true,
            None,
            None,
            true,
            id == Id::MJPEG,
        )?;
        Ok(Self {
            inner,
            backend: decoder_name.to_string(),
        })
    }

    /// The first hardware path that opens on this machine: named SoC decoders, then VA-API,
    /// CUDA and QSV devices. `None` means software decoding is the best option.
    pub fn best_available(input: FourCc) -> Option<Self> {
        // Only candidates whose hardware exists; checking that does not load FFmpeg.
        let named = named_candidates(input)
            .iter()
            .filter(|name| crate::ffmpeg::hw_presence::decoder_hardware(name, input))
            .filter_map(|name| Self::by_name(input, name).ok());
        let devices = hardware_devices()
            .into_iter()
            .filter_map(|device| Self::new(input, Some(device)).ok());
        named.chain(devices).find(|decoder| decoder.opens())
    }

    /// Whether this machine has any hardware [`FfmpegLumaDecoder::best_available`] could use for
    /// `input`, checked without loading FFmpeg.
    pub fn hardware_present(input: FourCc) -> bool {
        named_candidates(input)
            .iter()
            .any(|name| crate::ffmpeg::hw_presence::decoder_hardware(name, input))
            || !hardware_devices().is_empty()
    }

    /// Which decoder/device this instance uses, e.g. `mjpeg_rkmpp` or `mjpeg+Vaapi`.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Open the underlying decoder now so missing devices fail here rather than on the first
    /// frame.
    pub fn opens(&self) -> bool {
        let Ok(mut guard) = self.inner.state.lock() else {
            return false;
        };
        self.inner.decoder_state(&mut guard).is_ok()
    }
}

impl Codec for FfmpegLumaDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.inner.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.inner.process(input)
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.inner.process_shared(input, pool)
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        self.inner.memory_stats()
    }
}

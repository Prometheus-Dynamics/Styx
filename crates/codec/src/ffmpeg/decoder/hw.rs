//! Hardware decode devices for FFmpeg decoders (VA-API, CUDA/NVDEC, QSV, DRM).
//!
//! Decoded surfaces that live in device memory are transferred to system memory before Styx
//! converts them; DRM-PRIME surfaces are mapped directly (see `drm_prime`).

use std::ptr;

use crate::ffmpeg::ff::{codec, frame::Video as FfFrame, sys as ffi};

use crate::CodecError;

/// Hardware device used to accelerate an FFmpeg decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FfmpegHwDevice {
    /// VA-API (Intel iHD/i965, AMD radeonsi). JPEG decode on Intel Gen9+ and AMD VCN.
    Vaapi,
    /// NVIDIA NVDEC through CUDA (desktop GPUs). Jetson uses vendor decoders by name instead.
    Cuda,
    /// Intel Quick Sync Video.
    Qsv,
    /// Kernel DRM device; used by V4L2 request/M2M and Rockchip MPP decoders for DRM-PRIME.
    Drm,
}

impl FfmpegHwDevice {
    pub const ALL: [FfmpegHwDevice; 4] = [Self::Vaapi, Self::Cuda, Self::Qsv, Self::Drm];

    pub(super) fn av_type(self) -> ffi::AVHWDeviceType {
        match self {
            Self::Vaapi => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            Self::Cuda => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            Self::Qsv => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
            Self::Drm => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
        }
    }

    /// Whether this FFmpeg build can decode `codec` with this device and the device opens.
    pub fn is_available_for(self, codec: crate::ffmpeg::ff::Codec) -> bool {
        // SAFETY: `codec` is a valid FFmpeg codec pointer.
        if unsafe { hw_pix_fmt(codec, self.av_type()) }.is_none() {
            return false;
        }
        match create_device(self.av_type()) {
            Ok(device) => {
                // SAFETY: `device` was just created and is not shared.
                unsafe { ffi::av_buffer_unref(&mut { device }) };
                true
            }
            Err(_) => false,
        }
    }
}

/// Pixel format that `codec` produces when decoding through `device_type`.
pub(super) unsafe fn hw_pix_fmt(
    codec: crate::ffmpeg::ff::Codec,
    device_type: ffi::AVHWDeviceType,
) -> Option<ffi::AVPixelFormat> {
    let mut idx = 0;
    loop {
        // SAFETY: iterating hw configs of a valid codec until FFmpeg returns null.
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), idx) };
        if config.is_null() {
            return None;
        }
        // SAFETY: non-null config pointer owned by FFmpeg's static codec tables.
        let config = unsafe { &*config };
        let device_ctx = (config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) != 0;
        if device_ctx && config.device_type == device_type {
            return Some(config.pix_fmt);
        }
        idx += 1;
    }
}

/// Whether `codec` can emit DRM-PRIME frames without an external device context
/// (e.g. ffmpeg-rockchip's `*_rkmpp` decoders).
pub(super) unsafe fn internal_drm_prime(codec: crate::ffmpeg::ff::Codec) -> bool {
    let mut idx = 0;
    loop {
        // SAFETY: as in `hw_pix_fmt`.
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), idx) };
        if config.is_null() {
            return false;
        }
        // SAFETY: non-null config pointer owned by FFmpeg.
        let config = unsafe { &*config };
        let internal = (config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL) != 0;
        if internal && config.pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
            return true;
        }
        idx += 1;
    }
}

fn create_device(device_type: ffi::AVHWDeviceType) -> Result<*mut ffi::AVBufferRef, CodecError> {
    let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
    // SAFETY: FFmpeg writes a new reference into `device` on success.
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(&mut device, device_type, ptr::null(), ptr::null_mut(), 0)
    };
    if ret < 0 || device.is_null() {
        return Err(CodecError::Codec(format!(
            "ffmpeg hw device {device_type:?} unavailable: {}",
            crate::ffmpeg::ff::Error::from(ret)
        )));
    }
    Ok(device)
}

/// Attach `device` to the decoder and make `get_format` pick its surface format.
pub(super) unsafe fn configure_hw_device(
    context: &mut codec::Context,
    codec: crate::ffmpeg::ff::Codec,
    device: FfmpegHwDevice,
) -> Result<(), CodecError> {
    // SAFETY: `codec` is valid.
    let pix_fmt = unsafe { hw_pix_fmt(codec, device.av_type()) }
        .ok_or_else(|| CodecError::Codec(format!("ffmpeg decoder has no {device:?} hw config")))?;
    let device_ctx = create_device(device.av_type())?;
    // SAFETY: `context` is an unopened codec context we own; FFmpeg takes the device reference.
    unsafe {
        let ctx = context.as_mut_ptr();
        (*ctx).hw_device_ctx = device_ctx;
        (*ctx).opaque = pix_fmt as i64 as usize as *mut std::ffi::c_void;
        (*ctx).get_format = Some(prefer_opaque_format);
    }
    Ok(())
}

/// `get_format` callback choosing the format stored in `opaque`, else the first software one.
unsafe extern "C" fn prefer_opaque_format(
    ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: FFmpeg passes its own context; `opaque` was set by `configure_hw_device`.
    let wanted = unsafe { (*ctx).opaque } as usize as i64;
    let mut idx = 0usize;
    let mut first = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    loop {
        // SAFETY: the list is terminated by AV_PIX_FMT_NONE.
        let fmt = unsafe { *formats.add(idx) };
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return first;
        }
        if fmt as i64 == wanted {
            return fmt;
        }
        if first == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            first = fmt;
        }
        idx += 1;
    }
}

/// Copy a device-memory frame (VA-API, CUDA, QSV surface) into system memory.
/// Returns `None` for software and DRM-PRIME frames, which need no transfer.
pub(super) fn transfer_to_system(frame: &FfFrame) -> Result<Option<FfFrame>, CodecError> {
    // SAFETY: reading fields of a valid decoded frame.
    let hw_frames = unsafe { (*frame.as_ptr()).hw_frames_ctx };
    if hw_frames.is_null()
        || frame.format() == crate::ffmpeg::ff::util::format::pixel::Pixel::DRM_PRIME
    {
        return Ok(None);
    }
    let mut sw = FfFrame::empty();
    // SAFETY: both frames are valid; FFmpeg allocates `sw` in the frames context's sw format.
    let ret = unsafe { ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) };
    if ret < 0 {
        return Err(CodecError::Codec(format!(
            "ffmpeg hw frame transfer failed: {}",
            crate::ffmpeg::ff::Error::from(ret)
        )));
    }
    // SAFETY: copies pts and other properties between two valid frames.
    unsafe { ffi::av_frame_copy_props(sw.as_mut_ptr(), frame.as_ptr()) };
    Ok(Some(sw))
}

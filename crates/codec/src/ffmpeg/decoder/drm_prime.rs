use std::{
    os::fd::{FromRawFd, OwnedFd},
    ptr,
};

use crate::ffmpeg::ff::{codec, error::Error as FfmpegError, frame::Video as FfFrame, sys as ffi};
use styx_core::prelude::*;

use crate::CodecError;

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmObjectDescriptor {
    fd: i32,
    size: usize,
    format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmPlaneDescriptor {
    object_index: i32,
    offset: isize,
    pitch: isize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmLayerDescriptor {
    format: u32,
    nb_planes: i32,
    planes: [AvDrmPlaneDescriptor; 4],
}

#[repr(C)]
struct AvDrmFrameDescriptor {
    nb_objects: i32,
    objects: [AvDrmObjectDescriptor; 4],
    nb_layers: i32,
    layers: [AvDrmLayerDescriptor; 4],
}

#[derive(Clone, Debug)]
pub(super) struct DrmPrimePlane {
    pub(super) fd: i32,
    pub(super) offset: usize,
    pub(super) len: usize,
    pub(super) stride: usize,
}

pub(super) struct DrmPrimeDescriptor {
    pub(super) format: FourCc,
    pub(super) planes: Vec<DrmPrimePlane>,
    pub(super) backing_bytes: usize,
    /// DRM format modifier; only linear (0) buffers can be read by the CPU.
    pub(super) modifier: u64,
}

pub(super) struct FfmpegDrmPrimeBacking {
    pub(super) _frame: FfFrame,
    pub(super) planes: Vec<DrmPrimePlane>,
    pub(super) backing_bytes: usize,
    pub(super) cpu_readable: bool,
    pub(super) mapped: std::sync::OnceLock<Vec<MappedObject>>,
}

/// One mmap of a DRM object, covering every plane that lives in it.
pub(super) struct MappedObject {
    fd: i32,
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: mappings are read-only, created once through `OnceLock`, and unmapped in `Drop`.
unsafe impl Send for FfmpegDrmPrimeBacking {}
// SAFETY: see `Send`; plane data is only exposed as shared slices.
unsafe impl Sync for FfmpegDrmPrimeBacking {}

impl FfmpegDrmPrimeBacking {
    pub(super) fn new(frame: FfFrame, descriptor: DrmPrimeDescriptor) -> Self {
        Self {
            _frame: frame,
            planes: descriptor.planes,
            backing_bytes: descriptor.backing_bytes,
            // Tiled/compressed (AFBC, Intel Y-tile, NVIDIA block-linear) layouts are export-only.
            cpu_readable: descriptor.modifier == 0,
            mapped: std::sync::OnceLock::new(),
        }
    }

    fn mapped_objects(&self) -> &[MappedObject] {
        self.mapped.get_or_init(|| {
            let mut objects: Vec<MappedObject> = Vec::new();
            for plane in &self.planes {
                if objects.iter().any(|object| object.fd == plane.fd) {
                    continue;
                }
                let len = self
                    .planes
                    .iter()
                    .filter(|p| p.fd == plane.fd)
                    .map(|p| p.offset + p.len)
                    .max()
                    .unwrap_or(0);
                // SAFETY: read-only shared mapping of a dma-buf fd kept alive by `_frame`;
                // failure is detected via MAP_FAILED.
                let ptr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        len,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        plane.fd,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED || len == 0 {
                    continue;
                }
                let _ = styx_core::prelude::dmabuf_begin_cpu_read(plane.fd);
                objects.push(MappedObject {
                    fd: plane.fd,
                    ptr,
                    len,
                });
            }
            objects
        })
    }
}

impl Drop for FfmpegDrmPrimeBacking {
    fn drop(&mut self) {
        if let Some(objects) = self.mapped.take() {
            for object in objects {
                let _ = styx_core::prelude::dmabuf_end_cpu_read(object.fd);
                // SAFETY: `ptr`/`len` came from a successful mmap above and are unmapped once.
                unsafe { libc::munmap(object.ptr, object.len) };
            }
        }
    }
}

impl ExternalBacking for FfmpegDrmPrimeBacking {
    /// The whole DRM object holding plane `index`; plane layouts carry the offsets. `None` for
    /// tiled or compressed buffers, which only support dma-buf export.
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        if !self.cpu_readable {
            return None;
        }
        let fd = self.planes.get(index)?.fd;
        let object = self
            .mapped_objects()
            .iter()
            .find(|object| object.fd == fd)?;
        // SAFETY: the mapping lives as long as `self` and spans `len` readable bytes.
        Some(unsafe { std::slice::from_raw_parts(object.ptr.cast::<u8>(), object.len) })
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.backing_bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "ffmpeg_drm_prime"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn cpu_access(&self) -> CpuAccess {
        // GPU memory mapped for the CPU is typically write-combined.
        if self.cpu_readable {
            CpuAccess::Uncached
        } else {
            CpuAccess::None
        }
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in &self.planes {
            let fd = unsafe { libc::dup(plane.fd) };
            if fd < 0 {
                return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
            }
            planes.push(FrameFdPlane {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                offset: plane.offset,
                len: plane.len,
            });
        }
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }
}

pub(super) unsafe fn configure_drm_prime_decoder_context(
    context: &mut codec::Context,
    codec: crate::ffmpeg::ff::Codec,
) -> Result<(), CodecError> {
    if !unsafe { codec_supports_drm_prime_device_ctx(codec) } {
        // Decoders such as ffmpeg-rockchip's `*_rkmpp` produce DRM-PRIME internally without a
        // device context; they only need `get_format` to pick it.
        if unsafe { super::hw::internal_drm_prime(codec) } {
            unsafe { (*context.as_mut_ptr()).get_format = Some(prefer_drm_prime_format) };
            return Ok(());
        }
        return Err(CodecError::Codec(
            "ffmpeg decoder does not advertise DRM PRIME hw output".into(),
        ));
    }
    let mut device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Err(CodecError::Codec(format!(
            "ffmpeg DRM device creation failed: {}",
            FfmpegError::from(ret)
        )));
    }
    if device_ctx.is_null() {
        return Err(CodecError::Codec(
            "ffmpeg DRM device creation returned null".into(),
        ));
    }
    let ctx = unsafe { context.as_mut_ptr() };
    unsafe {
        (*ctx).hw_device_ctx = device_ctx;
        (*ctx).get_format = Some(prefer_drm_prime_format);
    }
    Ok(())
}

pub(super) unsafe fn drm_prime_descriptor_from_frame(
    frame: &FfFrame,
) -> Result<DrmPrimeDescriptor, CodecError> {
    let av_frame = unsafe { frame.as_ptr() };
    let ptr = unsafe { (*av_frame).data[0] };
    if ptr.is_null() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME frame missing descriptor".into(),
        ));
    }
    let desc = unsafe { &*(ptr.cast::<AvDrmFrameDescriptor>()) };
    drm_prime_descriptor_from_raw(desc, frame.height() as usize)
}

unsafe fn codec_supports_drm_prime_device_ctx(codec: crate::ffmpeg::ff::Codec) -> bool {
    let mut idx = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), idx) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        let has_device_ctx = (config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) != 0;
        if has_device_ctx
            && config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM
            && config.pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME
        {
            return true;
        }
        idx += 1;
    }
}

unsafe extern "C" fn prefer_drm_prime_format(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut idx = 0usize;
    let mut first = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    loop {
        let fmt = unsafe { *formats.add(idx) };
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return first;
        }
        if idx == 0 {
            first = fmt;
        }
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
            return fmt;
        }
        idx += 1;
    }
}

fn drm_prime_descriptor_from_raw(
    desc: &AvDrmFrameDescriptor,
    height: usize,
) -> Result<DrmPrimeDescriptor, CodecError> {
    if desc.nb_objects <= 0 || desc.nb_objects as usize > desc.objects.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid object count".into(),
        ));
    }
    if desc.nb_layers <= 0 || desc.nb_layers as usize > desc.layers.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid layer count".into(),
        ));
    }
    let layer = desc.layers[0];
    if layer.nb_planes <= 0 || layer.nb_planes as usize > layer.planes.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid plane count".into(),
        ));
    }
    let format = FourCc::new(layer.format.to_le_bytes());
    let object_count = desc.nb_objects as usize;
    let mut planes = Vec::with_capacity(layer.nb_planes as usize);
    for idx in 0..layer.nb_planes as usize {
        let plane = layer.planes[idx];
        if plane.object_index < 0 || plane.object_index as usize >= object_count {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane references invalid object".into(),
            ));
        }
        if plane.offset < 0 || plane.pitch <= 0 {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane has invalid layout".into(),
            ));
        }
        let object = desc.objects[plane.object_index as usize];
        let offset = plane.offset as usize;
        let stride = plane.pitch as usize;
        if offset > object.size {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane offset exceeds object size".into(),
            ));
        }
        let estimated = stride.saturating_mul(drm_plane_height(format, idx, height));
        let available = object.size.saturating_sub(offset);
        planes.push(DrmPrimePlane {
            fd: object.fd,
            offset,
            len: estimated.min(available),
            stride,
        });
    }
    let backing_bytes = desc.objects[..object_count]
        .iter()
        .map(|object| object.size)
        .sum();
    let modifier = desc.objects[layer.planes[0].object_index as usize].format_modifier;
    Ok(DrmPrimeDescriptor {
        format,
        planes,
        backing_bytes,
        modifier,
    })
}

fn drm_plane_height(format: FourCc, index: usize, height: usize) -> usize {
    match (&format.to_u32().to_le_bytes(), index) {
        (b"NV12" | b"NV21", 1) => height.div_ceil(2),
        (b"YU12" | b"YV12", 1 | 2) => height.div_ceil(2),
        _ => height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_prime_descriptor_parses_nv12_layout() {
        let desc = AvDrmFrameDescriptor {
            nb_objects: 1,
            objects: [
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 4096,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
            ],
            nb_layers: 1,
            layers: [
                AvDrmLayerDescriptor {
                    format: FourCc::NV12.to_u32(),
                    nb_planes: 2,
                    planes: [
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 640,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 2048,
                            pitch: 640,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 0,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 0,
                        },
                    ],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
            ],
        };

        let parsed = drm_prime_descriptor_from_raw(&desc, 4).expect("parse");
        assert_eq!(parsed.format, FourCc::NV12);
        assert_eq!(parsed.planes.len(), 2);
        assert_eq!(parsed.planes[0].len, 2560);
        assert_eq!(parsed.planes[1].offset, 2048);
        assert_eq!(parsed.planes[1].len, 1280);
        assert_eq!(parsed.backing_bytes, 4096);
    }

    #[test]
    fn prefer_drm_prime_format_picks_drm_when_offered() {
        let formats = [
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
            ffi::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        let picked = unsafe { prefer_drm_prime_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(picked, ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME);
    }

    #[test]
    fn prefer_drm_prime_format_falls_back_to_first() {
        let formats = [
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            ffi::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        let picked = unsafe { prefer_drm_prime_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(picked, ffi::AVPixelFormat::AV_PIX_FMT_NV12);
    }
}

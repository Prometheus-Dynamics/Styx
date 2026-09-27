//! Encoders that take device surfaces (VA-API: `h264_vaapi`, `hevc_vaapi`).
//!
//! A frame in a dma-buf (a camera buffer from V4L2 or libcamera) is imported as the surface
//! itself: the encoder reads the camera's memory and no pixel is copied. Other frames are
//! uploaded to a surface straight from their own memory. Imports need a layout the GPU accepts
//! (AMD wants 256-byte row pitches, for example); frames that do not qualify are uploaded.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;

use styx_core::prelude::*;

use super::encoder_input::Owner;
use crate::CodecError;
use crate::ffmpeg::ff::{Codec, frame::Video as FfFrame, sys as ffi};

/// `DRM_FORMAT_NV12` (drm_fourcc.h).
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// Device type and surface format of an encoder that only takes device surfaces.
pub(super) fn surface_config(codec: Codec) -> Option<(ffi::AVHWDeviceType, ffi::AVPixelFormat)> {
    for index in 0.. {
        // SAFETY: iterating the hw configs of a valid codec until FFmpeg returns null.
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), index) };
        if config.is_null() {
            return None;
        }
        // SAFETY: non-null config owned by FFmpeg's static codec tables.
        let config = unsafe { &*config };
        if config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX != 0
            && config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
        {
            return Some((config.device_type, config.pix_fmt));
        }
    }
    None
}

fn check(ret: i32, what: &str) -> Result<(), CodecError> {
    if ret < 0 {
        Err(CodecError::Codec(format!(
            "ffmpeg {what}: {}",
            crate::ffmpeg::ff::Error::from(ret)
        )))
    } else {
        Ok(())
    }
}

/// A frames context of `format` (a device surface format) holding `sw_format` pixels.
///
/// # Safety
/// `device` must be a valid hardware device reference.
unsafe fn frames_context(
    device: *mut ffi::AVBufferRef,
    format: ffi::AVPixelFormat,
    sw_format: ffi::AVPixelFormat,
    width: u32,
    height: u32,
) -> Result<*mut ffi::AVBufferRef, CodecError> {
    // SAFETY: valid device; FFmpeg returns a new frames context or null.
    let mut frames = unsafe { ffi::av_hwframe_ctx_alloc(device) };
    if frames.is_null() {
        return Err(CodecError::Codec("ffmpeg hw frames context".into()));
    }
    // SAFETY: a fresh AVHWFramesContext, configured before init (size 0: allocate on demand).
    unsafe {
        let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
        (*ctx).format = format;
        (*ctx).sw_format = sw_format;
        (*ctx).width = width as i32;
        (*ctx).height = height as i32;
        if let Err(err) = check(ffi::av_hwframe_ctx_init(frames), "hw frames init") {
            ffi::av_buffer_unref(&mut frames);
            return Err(err);
        }
    }
    Ok(frames)
}

/// A `device_type` device, and the DRM device it was derived from when there is one (needed
/// to import dma-bufs; VA-API cannot derive a DRM device, only the reverse). Render nodes are
/// tried in order (FFmpeg's default only tries the first, which on multi-GPU machines may not
/// support the API), then FFmpeg's default.
fn open_device(
    device_type: ffi::AVHWDeviceType,
) -> Result<(*mut ffi::AVBufferRef, Option<*mut ffi::AVBufferRef>), CodecError> {
    let mut paths: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    paths.sort();
    for path in paths {
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        let (mut drm, mut device) = (ptr::null_mut(), ptr::null_mut());
        let drm_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM;
        // SAFETY: FFmpeg writes new device references on success.
        unsafe {
            if ffi::av_hwdevice_ctx_create(&mut drm, drm_type, path.as_ptr(), ptr::null_mut(), 0)
                < 0
            {
                continue;
            }
            if ffi::av_hwdevice_ctx_create_derived(&mut device, device_type, drm, 0) >= 0 {
                return Ok((device, Some(drm)));
            }
            ffi::av_buffer_unref(&mut drm);
        }
    }
    let mut device = ptr::null_mut();
    // SAFETY: FFmpeg writes a new device reference on success.
    check(
        unsafe {
            ffi::av_hwdevice_ctx_create(&mut device, device_type, ptr::null(), ptr::null_mut(), 0)
        },
        "hw device",
    )?;
    Ok((device, None))
}

/// Device surfaces for one encoder.
pub(super) struct Surfaces {
    /// Pool the encoder is opened with and uploads go to.
    frames: *mut ffi::AVBufferRef,
    device: *mut ffi::AVBufferRef,
    /// The DRM device `device` was derived from, for importing dma-bufs.
    drm_device: Option<*mut ffi::AVBufferRef>,
    width: u32,
    height: u32,
    import: Import,
}

enum Import {
    Untried,
    /// DRM-PRIME frames context, and the surface context derived from it for mapping.
    Ready {
        drm: *mut ffi::AVBufferRef,
        mapped: *mut ffi::AVBufferRef,
    },
    Unavailable,
}

// SAFETY: the contexts are reference-counted FFmpeg objects, used under the encoder's lock.
unsafe impl Send for Surfaces {}

impl Surfaces {
    /// Surfaces of `format` for NV12 pixels at `width`x`height` on a new `device_type` device.
    pub(super) fn new(
        device_type: ffi::AVHWDeviceType,
        format: ffi::AVPixelFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        let (mut device, mut drm_device) = open_device(device_type)?;
        let nv12 = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        // SAFETY: `device` was just created.
        match unsafe { frames_context(device, format, nv12, width, height) } {
            Ok(frames) => Ok(Self {
                frames,
                device,
                drm_device,
                width,
                height,
                import: Import::Untried,
            }),
            Err(err) => {
                // SAFETY: our references.
                unsafe {
                    ffi::av_buffer_unref(&mut device);
                    if let Some(drm) = drm_device.as_mut() {
                        ffi::av_buffer_unref(drm);
                    }
                }
                Err(err)
            }
        }
    }

    pub(super) fn frames(&self) -> *mut ffi::AVBufferRef {
        self.frames
    }

    /// A new surface holding `sw` (NV12 at the surfaces' size).
    pub(super) fn upload(&self, sw: &FfFrame) -> Result<FfFrame, CodecError> {
        let mut surface = FfFrame::empty();
        // SAFETY: valid frames context and frames; FFmpeg copies `sw` into the new surface.
        unsafe {
            check(
                ffi::av_hwframe_get_buffer(self.frames, surface.as_mut_ptr(), 0),
                "hw surface",
            )?;
            check(
                ffi::av_hwframe_transfer_data(surface.as_mut_ptr(), sw.as_ptr(), 0),
                "hw upload",
            )?;
        }
        Ok(surface)
    }

    /// `frame`'s own dma-buf as a surface, without copying, when it is an NV12 dma-buf the
    /// device imports. `owner` keeps the buffer (and its place out of the driver's queue) until
    /// the encoder is done with it.
    pub(super) fn import(&mut self, frame: &FrameLease, owner: &Owner) -> Option<FfFrame> {
        let meta = frame.meta();
        let res = meta.format.resolution;
        if meta.format.code != FourCc::NV12
            || (res.width.get(), res.height.get()) != (self.width, self.height)
        {
            return None;
        }
        let (drm, mapped) = self.import_contexts()?;
        let FrameBackingExport::DmabufPlanes { planes: fds } = frame.export_backing().ok()? else {
            return None;
        };
        let descriptor = drm_descriptor(frame, &fds)?;
        let drm_frame = drm_frame(descriptor, fds, owner.clone(), drm, res)?;
        let mut surface = FfFrame::empty();
        // SAFETY: valid frames; the mapped surface keeps a reference to `drm_frame`.
        unsafe {
            (*surface.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*surface.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(mapped);
            let flags = ffi::AV_HWFRAME_MAP_READ | ffi::AV_HWFRAME_MAP_DIRECT;
            if ffi::av_hwframe_map(surface.as_mut_ptr(), drm_frame.as_ptr(), flags) < 0 {
                return None;
            }
        }
        #[cfg(test)]
        tests::IMPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(surface)
    }

    fn import_contexts(&mut self) -> Option<(*mut ffi::AVBufferRef, *mut ffi::AVBufferRef)> {
        if let Import::Untried = self.import {
            // SAFETY: valid device; each step either succeeds or leaves nothing to free.
            // Without import support frames are uploaded instead.
            self.import = unsafe { self.create_import() }.unwrap_or(Import::Unavailable);
        }
        match self.import {
            Import::Ready { drm, mapped } => Some((drm, mapped)),
            _ => None,
        }
    }

    unsafe fn create_import(&self) -> Result<Import, CodecError> {
        let drm_device = self
            .drm_device
            .ok_or_else(|| CodecError::Codec("no DRM device to import dma-bufs".into()))?;
        // SAFETY: valid DRM device.
        let drm = unsafe {
            frames_context(
                drm_device,
                ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                self.width,
                self.height,
            )
        };
        let mut drm = drm?;
        let mut mapped: *mut ffi::AVBufferRef = ptr::null_mut();
        // SAFETY: surfaces on our device, mapped from DRM-PRIME frames of `drm`.
        let derived = check(
            unsafe {
                ffi::av_hwframe_ctx_create_derived(
                    &mut mapped,
                    ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                    self.device,
                    drm,
                    ffi::AV_HWFRAME_MAP_READ | ffi::AV_HWFRAME_MAP_DIRECT,
                )
            },
            "derive mapped surfaces",
        );
        if let Err(err) = derived {
            // SAFETY: our reference.
            unsafe { ffi::av_buffer_unref(&mut drm) };
            return Err(err);
        }
        Ok(Import::Ready { drm, mapped })
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        // SAFETY: our references, each released once.
        unsafe {
            if let Import::Ready { drm, mapped } = &mut self.import {
                ffi::av_buffer_unref(mapped);
                ffi::av_buffer_unref(drm);
            }
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
            if let Some(drm) = self.drm_device.as_mut() {
                ffi::av_buffer_unref(drm);
            }
        }
    }
}

/// The DRM-PRIME description of an NV12 frame in the dma-bufs `fds` (one per plane, or one for
/// both), checked against each buffer's real size.
fn drm_descriptor(frame: &FrameLease, fds: &[FrameFdPlane]) -> Option<ffi::AVDRMFrameDescriptor> {
    let layouts = frame.layouts();
    let height = frame.meta().format.resolution.height.get() as usize;
    if layouts.len() != 2 || fds.is_empty() {
        return None;
    }
    // SAFETY: a plain C struct; all-zero is a valid empty descriptor.
    let mut desc: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
    let mut objects: Vec<i32> = Vec::new();
    desc.nb_layers = 1;
    desc.layers[0].format = DRM_FORMAT_NV12;
    desc.layers[0].nb_planes = 2;
    for (index, layout) in layouts.iter().enumerate() {
        let fd = &fds[index.min(fds.len() - 1)];
        let raw = fd.fd.as_raw_fd();
        // SAFETY: querying the size of an open dma-buf leaves it unchanged.
        let size = unsafe { libc::lseek(raw, 0, libc::SEEK_END) };
        let object = match objects.iter().position(|&o| o == raw) {
            Some(object) => object,
            None => {
                let object = objects.len();
                objects.push(raw);
                desc.objects[object].fd = raw;
                desc.objects[object].size = usize::try_from(size).ok()?;
                object
            }
        };
        let offset = fd.offset.checked_add(layout.offset)?;
        let rows = if index == 0 {
            height
        } else {
            height.div_ceil(2)
        };
        if offset.checked_add(layout.stride.checked_mul(rows)?)? > desc.objects[object].size {
            return None;
        }
        desc.layers[0].planes[index].object_index = object as i32;
        desc.layers[0].planes[index].offset = offset as isize;
        desc.layers[0].planes[index].pitch = layout.stride as isize;
    }
    desc.nb_objects = objects.len() as i32;
    Some(desc)
}

/// A DRM-PRIME frame of `descriptor` in the frames context `drm`, owning the descriptor, the
/// dma-buf fds and `owner` until FFmpeg releases it.
fn drm_frame(
    descriptor: ffi::AVDRMFrameDescriptor,
    fds: Vec<FrameFdPlane>,
    owner: Owner,
    drm: *mut ffi::AVBufferRef,
    res: Resolution,
) -> Option<FfFrame> {
    struct Held {
        descriptor: ffi::AVDRMFrameDescriptor,
        _fds: Vec<OwnedFd>,
        _owner: Owner,
    }
    unsafe extern "C" fn release(opaque: *mut c_void, _data: *mut u8) {
        // SAFETY: `opaque` is the boxed `Held` below, released exactly once.
        drop(unsafe { Box::from_raw(opaque.cast::<Held>()) });
    }
    let held = Box::into_raw(Box::new(Held {
        descriptor,
        _fds: fds.into_iter().map(|plane| plane.fd).collect(),
        _owner: owner,
    }));
    // SAFETY: `held` lives until `release`; FFmpeg frees the buffer once.
    let buf = unsafe {
        ffi::av_buffer_create(
            ptr::addr_of_mut!((*held).descriptor).cast(),
            std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
            Some(release),
            held.cast(),
            0,
        )
    };
    if buf.is_null() {
        // SAFETY: FFmpeg did not take `held`.
        unsafe { release(held.cast(), ptr::null_mut()) };
        return None;
    }
    let mut frame = FfFrame::empty();
    // SAFETY: a fresh frame taking ownership of `buf` and a new reference to `drm`.
    unsafe {
        let raw = frame.as_mut_ptr();
        (*raw).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*raw).width = res.width.get() as i32;
        (*raw).height = res.height.get() as i32;
        (*raw).buf[0] = buf;
        (*raw).data[0] = (*buf).data;
        (*raw).hw_frames_ctx = ffi::av_buffer_ref(drm);
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsFd, FromRawFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use smallvec::smallvec;

    use super::*;
    use crate::Codec;
    use crate::ffmpeg::encoder::FfmpegH264Encoder;

    /// Frames imported as device surfaces.
    pub(super) static IMPORTED: AtomicUsize = AtomicUsize::new(0);

    const W: usize = 1280;
    const H: usize = 720;

    /// An NV12 frame in a dma-buf from the system heap, like a camera buffer.
    struct DmaBuf {
        fd: OwnedFd,
        map: *mut u8,
        len: usize,
    }

    // SAFETY: a shared read-only mapping of a dma-buf we own.
    unsafe impl Send for DmaBuf {}
    // SAFETY: as above.
    unsafe impl Sync for DmaBuf {}

    impl DmaBuf {
        fn allocate(len: usize) -> Option<Self> {
            #[repr(C)]
            struct Alloc {
                len: u64,
                fd: u32,
                fd_flags: u32,
                heap_flags: u64,
            }
            const DMA_HEAP_IOCTL_ALLOC: libc::c_ulong = 0xC018_4800;
            let heap = std::fs::File::open("/dev/dma_heap/system").ok()?;
            let mut alloc = Alloc {
                len: len as u64,
                fd: 0,
                fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
                heap_flags: 0,
            };
            // SAFETY: the heap ioctl with a correctly sized request.
            if unsafe { libc::ioctl(heap.as_raw_fd(), DMA_HEAP_IOCTL_ALLOC as _, &mut alloc) } != 0
            {
                return None;
            }
            // SAFETY: the kernel returned a new dma-buf fd.
            let fd = unsafe { OwnedFd::from_raw_fd(alloc.fd as i32) };
            // SAFETY: mapping the whole dma-buf.
            let map = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            };
            (map != libc::MAP_FAILED).then(|| Self {
                fd,
                map: map.cast(),
                len,
            })
        }
    }

    impl Drop for DmaBuf {
        fn drop(&mut self) {
            // SAFETY: our mapping.
            unsafe { libc::munmap(self.map.cast(), self.len) };
        }
    }

    impl ExternalBacking for DmaBuf {
        fn plane_data(&self, _index: usize) -> Option<&[u8]> {
            // SAFETY: mapped for our lifetime.
            Some(unsafe { std::slice::from_raw_parts(self.map, self.len) })
        }

        fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
            let fd = self
                .fd
                .as_fd()
                .try_clone_to_owned()
                .map_err(|_| FrameExportError::NotExportable)?;
            Ok(Some(FrameBackingExport::DmabufPlanes {
                planes: vec![FrameFdPlane {
                    fd,
                    offset: 0,
                    len: self.len,
                }],
            }))
        }
    }

    /// Frame `index` of a moving gradient, in a dma-buf or in host memory.
    fn frame(index: usize, dmabuf: bool) -> Option<FrameLease> {
        let len = W * H * 3 / 2;
        let pixels: Vec<u8> = (0..len)
            .map(|i| (i % W + i / W * 2 + index * 5) as u8)
            .collect();
        let res = Resolution::new(W as u32, H as u32)?;
        let meta = FrameMeta::new(MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709), 0);
        let layouts = smallvec![
            PlaneLayout {
                offset: 0,
                len: W * H,
                stride: W
            },
            PlaneLayout {
                offset: W * H,
                len: W * H / 2,
                stride: W
            },
        ];
        if dmabuf {
            let buf = DmaBuf::allocate(len)?;
            // SAFETY: our fresh mapping of `len` bytes.
            unsafe { std::slice::from_raw_parts_mut(buf.map, len) }.copy_from_slice(&pixels);
            return Some(FrameLease::from_external(meta, layouts, Arc::new(buf)));
        }
        let pool = BufferPool::with_limits(2, W * H, 0);
        let (mut y, mut uv) = (pool.lease(), pool.lease());
        y.resize(W * H);
        y.as_mut_slice().copy_from_slice(&pixels[..W * H]);
        uv.resize(W * H / 2);
        uv.as_mut_slice().copy_from_slice(&pixels[W * H..]);
        let layouts = smallvec![
            PlaneLayout {
                offset: 0,
                len: W * H,
                stride: W
            },
            PlaneLayout {
                offset: 0,
                len: W * H / 2,
                stride: W
            },
        ];
        Some(FrameLease::multi_plane(meta, smallvec![y, uv], layouts))
    }

    fn encode(dmabuf: bool) -> Option<Vec<u8>> {
        let encoder = FfmpegH264Encoder::new_vaapi_nv12().ok()?;
        let mut stream = Vec::new();
        for index in 0..10 {
            match encoder.process(frame(index, dmabuf)?) {
                Ok(packet) => stream.extend_from_slice(packet.planes()[0].data()),
                Err(crate::CodecError::Backpressure) => {}
                Err(err) => panic!("encode failed: {err}"),
            }
        }
        Some(stream)
    }

    #[test]
    fn dmabufs_are_encoded_without_copying_with_the_same_result() {
        // Skipped without a VA-API encoder or a dma-buf heap.
        let Some(uploaded) = encode(false) else {
            return;
        };
        let before = IMPORTED.load(Ordering::Relaxed);
        let Some(imported) = encode(true) else {
            return;
        };
        assert_eq!(
            IMPORTED.load(Ordering::Relaxed) - before,
            10,
            "every dma-buf imported"
        );
        assert!(!imported.is_empty());
        assert_eq!(
            imported, uploaded,
            "same bitstream as uploading the same pixels"
        );
    }
}

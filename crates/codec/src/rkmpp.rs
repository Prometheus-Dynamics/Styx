//! Hardware encoding on Rockchip VPUs through MPP (`librockchip_mpp`, the vendor BSP's
//! `/dev/mpp_service`): H.264 and H.265 on rkvenc/vepu, JPEG on the JPEG encoder, from NV12
//! or YUYV ([`RkMppEncoder`]). Written, untested on board.
//!
//! MPP is opened with `dlopen` on first use. A frame over one dma-buf at a layout MPP reads
//! (the ISP's NV12: luma at the start, chroma right after at the same stride) is imported by
//! descriptor and encoded in place; other frames are copied into dma-heap staging buffers
//! imported once. Each call puts one frame and takes its packet: MPP encodes synchronously
//! with the output timeout set, so nothing the device holds outlives a call.

mod ffi;

use std::ffi::{CString, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf, DmaHeap};

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use ffi::Handle;

/// How an MPP encoder encodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RkMppOptions {
    /// H.264/H.265: the target bit rate in bits per second (`None`: about 1/8 bit per pixel
    /// per second, variable).
    pub bitrate: Option<u32>,
    /// H.264/H.265: frames from one keyframe to the next (`None`: two seconds).
    pub gop: Option<u32>,
    /// The frame rate the rate control plans for.
    pub fps: u32,
    /// JPEG quality, 1 to 99.
    pub quality: u8,
    /// How long to wait for a packet.
    pub timeout: Duration,
    /// Encode frames over a dma-buf in place when MPP reads their layout; `false` copies every
    /// frame into a staging buffer.
    pub import_dmabuf: bool,
}

impl Default for RkMppOptions {
    fn default() -> Self {
        Self {
            bitrate: None,
            gop: None,
            fps: 30,
            quality: 85,
            timeout: Duration::from_secs(1),
            import_dmabuf: true,
        }
    }
}

fn mpp_err(context: &str, e: impl std::fmt::Display) -> CodecError {
    CodecError::Codec(format!("rkmpp {context}: {e}"))
}

fn check(context: &str, ret: c_int) -> Result<(), CodecError> {
    if ret == 0 {
        Ok(())
    } else {
        Err(mpp_err(context, format!("MPP error {ret}")))
    }
}

/// MPP's format for `code`, and bytes per pixel of its first plane.
fn mpp_format(code: FourCc) -> Option<(c_int, u32)> {
    match &code.to_u32().to_le_bytes() {
        b"NV12" => Some((ffi::FMT_YUV420SP, 1)),
        b"YUYV" => Some((ffi::FMT_YUV422_YUYV, 2)),
        _ => None,
    }
}

fn mpp_coding(code: FourCc) -> Option<c_int> {
    match &code.to_u32().to_le_bytes() {
        b"H264" => Some(ffi::CODING_AVC),
        b"H265" | b"HEVC" => Some(ffi::CODING_HEVC),
        b"MJPG" | b"JPEG" => Some(ffi::CODING_MJPEG),
        _ => None,
    }
}

/// One plane of a dma-buf frame: whether it is in the same dma-buf as the first plane, its
/// offset there and its stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaneAt {
    same: bool,
    offset: usize,
    stride: usize,
}

/// MPP's `(hor_stride, ver_stride)` for a frame of `height` rows whose planes lie at
/// `planes`, if MPP reads it in place: the first plane at the dma-buf's start, NV12's chroma
/// after a whole number of luma rows at the luma stride. `hor_stride` is in bytes.
fn import_strides(code: FourCc, height: usize, planes: &[PlaneAt]) -> Option<(u32, u32)> {
    let luma = planes.first()?;
    if luma.offset != 0 || luma.stride == 0 {
        return None;
    }
    let ver = match (&code.to_u32().to_le_bytes(), planes) {
        (b"YUYV", [_]) => height,
        (b"NV12", [_, chroma]) => {
            let rows = chroma.offset / luma.stride;
            let whole = chroma.offset % luma.stride == 0;
            if !chroma.same || !whole || chroma.stride != luma.stride || rows < height {
                return None;
            }
            rows
        }
        _ => return None,
    };
    Some((u32::try_from(luma.stride).ok()?, u32::try_from(ver).ok()?))
}

/// The strides of a staging buffer for `width` x `height`: rows and the luma height aligned
/// to 16, as MPP's encoders want.
fn staging_strides(bytes_per_pixel: u32, width: u32, height: u32) -> (u32, u32) {
    (
        width.next_multiple_of(16) * bytes_per_pixel,
        height.next_multiple_of(16),
    )
}

/// A dma-heap buffer frames are copied into, imported into MPP once.
struct Staging {
    buf: DmaBuf,
    map: Mapping,
    mpp: Handle,
}

/// An open MPP encoder context for one frame size and layout.
struct Session {
    mpp: &'static ffi::Mpp,
    ctx: Handle,
    api: *mut ffi::MppApi,
    key: (u32, u32, u32, u32),
    staging: Vec<Staging>,
    next: usize,
}

// SAFETY: the context and buffers are used by one thread at a time, under the encoder's lock.
unsafe impl Send for Session {}

impl Session {
    fn control(&self, cmd: c_int, param: *mut c_void) -> c_int {
        // SAFETY: `api` is the context's table from `mpp_create`.
        match unsafe { (*self.api).control } {
            // SAFETY: as MPP's headers declare it, with this context.
            Some(f) => unsafe { f(self.ctx, cmd, param) },
            None => -1,
        }
    }

    fn open(
        coding: c_int,
        format: c_int,
        (width, height): (u32, u32),
        (hor, ver): (u32, u32),
        options: &RkMppOptions,
    ) -> Result<Self, CodecError> {
        let mpp = ffi::mpp().map_err(|e| mpp_err("load", e))?;
        let (mut ctx, mut api) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: out-pointers to a context and its table.
        check("create", unsafe { (mpp.mpp_create)(&mut ctx, &mut api) })?;
        let session = Self {
            mpp,
            ctx,
            api,
            key: (width, height, hor, ver),
            staging: Vec::new(),
            next: 0,
        };
        let mut timeout = options.timeout.as_millis().min(i64::MAX as u128) as i64;
        let _ = session.control(ffi::SET_OUTPUT_TIMEOUT, (&raw mut timeout).cast());
        // SAFETY: a context from `mpp_create`, initialised once.
        check("init", unsafe { (mpp.mpp_init)(ctx, ffi::CTX_ENC, coding) })?;
        let mut cfg = ptr::null_mut();
        // SAFETY: an out-pointer to a configuration.
        check("config", unsafe { (mpp.mpp_enc_cfg_init)(&mut cfg) })?;
        let fps = options.fps.max(1) as i32;
        let jpeg = coding == ffi::CODING_MJPEG;
        let bps = options
            .bitrate
            .unwrap_or_else(|| (width * height).saturating_mul(fps as u32) / 8)
            .min(i32::MAX as u32) as i32;
        let mut keys: Vec<(&str, i32)> = vec![
            ("codec:type", coding),
            ("prep:width", width as i32),
            ("prep:height", height as i32),
            ("prep:hor_stride", hor as i32),
            ("prep:ver_stride", ver as i32),
            ("prep:format", format),
            ("rc:fps_in_flex", 0),
            ("rc:fps_in_num", fps),
            ("rc:fps_in_denom", 1),
            ("rc:fps_out_flex", 0),
            ("rc:fps_out_num", fps),
            ("rc:fps_out_denom", 1),
        ];
        if jpeg {
            let q = i32::from(options.quality.clamp(1, 99));
            keys.extend([
                ("rc:mode", ffi::RC_MODE_FIXQP),
                ("jpeg:q_factor", q),
                ("jpeg:qf_max", q),
                ("jpeg:qf_min", q),
            ]);
        } else {
            let mode = if options.bitrate.is_some() {
                ffi::RC_MODE_CBR
            } else {
                ffi::RC_MODE_VBR
            };
            let gop = options.gop.unwrap_or(fps as u32 * 2).min(i32::MAX as u32) as i32;
            keys.extend([
                ("rc:mode", mode),
                ("rc:bps_target", bps),
                ("rc:bps_max", bps.saturating_mul(17) / 16),
                ("rc:bps_min", bps / 16 * 15),
                ("rc:gop", gop),
            ]);
        }
        let mut failed = None;
        for (key, value) in keys {
            let name = CString::new(key).expect("key");
            // SAFETY: a configuration from `mpp_enc_cfg_init` and a NUL-terminated key.
            if unsafe { (mpp.mpp_enc_cfg_set_s32)(cfg, name.as_ptr(), value) } != 0 {
                failed.get_or_insert(key);
            }
        }
        let set = session.control(ffi::ENC_SET_CFG, cfg);
        // SAFETY: the configuration from `mpp_enc_cfg_init`, not used after.
        unsafe { (mpp.mpp_enc_cfg_deinit)(cfg) };
        if let Some(key) = failed {
            return Err(mpp_err("config", format!("MPP does not take {key}")));
        }
        check("config", set)?;
        if !jpeg {
            let mut mode = ffi::HEADER_MODE_EACH_IDR;
            let _ = session.control(ffi::ENC_SET_HEADER_MODE, (&raw mut mode).cast());
        }
        Ok(session)
    }

    /// Imports `fd` (`size` bytes) as an MPP buffer.
    fn import(&self, fd: c_int, size: usize) -> Result<Handle, CodecError> {
        let mut info = ffi::BufferInfo {
            kind: ffi::BUFFER_TYPE_EXT_DMA,
            size,
            ptr: ptr::null_mut(),
            hnd: ptr::null_mut(),
            fd,
            index: -1,
        };
        let mut buffer = ptr::null_mut();
        // SAFETY: a filled-in buffer description and an out-pointer; no group (MPP's default).
        check("import", unsafe {
            (self.mpp.mpp_buffer_import_with_tag)(
                ptr::null_mut(),
                &mut info,
                &mut buffer,
                ffi::TAG.as_ptr(),
                ffi::TAG.as_ptr(),
            )
        })?;
        Ok(buffer)
    }

    fn put_buffer(&self, buffer: Handle) {
        // SAFETY: a buffer from `mpp_buffer_import_with_tag`, put once.
        unsafe { (self.mpp.mpp_buffer_put_with_caller)(buffer, ffi::TAG.as_ptr()) };
    }

    /// The next staging buffer, made on first use (two in turn).
    fn staging(&mut self, size: usize) -> Result<&mut Staging, CodecError> {
        const KEEP: usize = 2;
        if self.staging.len() < KEEP {
            let heap = DmaHeap::open("system").map_err(|e| mpp_err("staging", e))?;
            let buf = heap.allocate(size).map_err(|e| mpp_err("staging", e))?;
            let map = buf.map().map_err(|e| mpp_err("staging", e))?;
            use std::os::fd::{AsFd, AsRawFd};
            let mpp = self.import(buf.as_fd().as_raw_fd(), size)?;
            self.staging.push(Staging { buf, map, mpp });
        }
        let i = self.next % self.staging.len();
        self.next += 1;
        Ok(&mut self.staging[i])
    }

    /// Encodes one frame in `buffer` and returns its packet as `(bytes, intra)` to `take`.
    fn encode<T>(
        &self,
        buffer: Handle,
        format: c_int,
        pts: i64,
        take: impl FnOnce(&[u8], bool) -> T,
    ) -> Result<T, CodecError> {
        let m = self.mpp;
        let (width, height, hor, ver) = self.key;
        let mut frame = ptr::null_mut();
        // SAFETY: an out-pointer; the frame is set up, put, then freed below.
        check("frame", unsafe { (m.mpp_frame_init)(&mut frame) })?;
        // SAFETY: a frame from `mpp_frame_init` and a buffer MPP knows.
        unsafe {
            (m.mpp_frame_set_width)(frame, width);
            (m.mpp_frame_set_height)(frame, height);
            (m.mpp_frame_set_hor_stride)(frame, hor);
            (m.mpp_frame_set_ver_stride)(frame, ver);
            (m.mpp_frame_set_fmt)(frame, format);
            (m.mpp_frame_set_buffer)(frame, buffer);
            (m.mpp_frame_set_pts)(frame, pts);
        }
        // SAFETY: the context's table from `mpp_create`.
        let (put, get) = unsafe { ((*self.api).encode_put_frame, (*self.api).encode_get_packet) };
        let (Some(put), Some(get)) = (put, get) else {
            // SAFETY: the frame from `mpp_frame_init`.
            unsafe { (m.mpp_frame_deinit)(&mut frame) };
            return Err(mpp_err("encode", "no encode functions"));
        };
        // SAFETY: this context and frame; MPP keeps its own reference to the frame.
        let ret = unsafe { put(self.ctx, frame) };
        // SAFETY: the frame from `mpp_frame_init`, not used after.
        unsafe { (m.mpp_frame_deinit)(&mut frame) };
        check("put frame", ret)?;
        let mut packet = ptr::null_mut();
        // SAFETY: this context and an out-pointer.
        check("get packet", unsafe { get(self.ctx, &mut packet) })?;
        if packet.is_null() {
            return Err(CodecError::Backpressure);
        }
        // SAFETY: a packet from `encode_get_packet`: its bytes are valid until it is freed.
        let (data, intra) = unsafe {
            let pos = (m.mpp_packet_get_pos)(packet).cast::<u8>();
            let len = (m.mpp_packet_get_length)(packet);
            let meta = (m.mpp_packet_get_meta)(packet);
            let mut intra = 0;
            if !meta.is_null() {
                (m.mpp_meta_get_s32)(meta, ffi::KEY_OUTPUT_INTRA, &mut intra);
            }
            let data = if pos.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(pos, len)
            };
            (data, intra != 0)
        };
        let out = take(data, intra);
        // SAFETY: the packet from `encode_get_packet`, freed once after its bytes were read.
        unsafe { (m.mpp_packet_deinit)(&mut packet) };
        Ok(out)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for s in self.staging.drain(..) {
            // SAFETY: a buffer imported in `staging`, put once, before its dma-buf closes.
            unsafe { (self.mpp.mpp_buffer_put_with_caller)(s.mpp, ffi::TAG.as_ptr()) };
            drop((s.map, s.buf));
        }
        // SAFETY: the context from `mpp_create`, destroyed once.
        unsafe { (self.mpp.mpp_destroy)(self.ctx) };
    }
}

/// An MPP hardware encoder: NV12 or YUYV frames in, one H.264, H.265 or JPEG packet out per
/// frame.
pub struct RkMppEncoder {
    descriptor: CodecDescriptor,
    options: RkMppOptions,
    session: Mutex<Option<Session>>,
    force_key: AtomicBool,
    pool: BufferPool,
}

impl RkMppEncoder {
    /// An encoder from `input` frames (`NV12`, `YUYV`) to `output` packets (`H264`, `H265`,
    /// `MJPG`). MPP opens on the first frame.
    pub fn new(input: FourCc, output: FourCc, options: RkMppOptions) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output,
                name: "rkmpp",
                impl_name: "rkmpp",
            },
            options,
            session: Mutex::new(None),
            force_key: AtomicBool::new(false),
            pool: BufferPool::lazy(
                crate::DEFAULT_CODEC_POOL_CHUNK_BYTES,
                crate::DEFAULT_CODEC_POOL_SPARE,
            ),
        }
    }

    /// Whether MPP is here (the library and `/dev/mpp_service`) and encodes `input` to
    /// `output`.
    pub fn available(input: FourCc, output: FourCc) -> bool {
        mpp_format(input).is_some()
            && mpp_coding(output).is_some()
            && Path::new("/dev/mpp_service").exists()
            && ffi::mpp().is_ok()
    }

    /// The input's layout for MPP if it is encoded in place: `(fd, size, hor, ver)`.
    fn in_place(&self, input: &FrameLease) -> Option<(c_int, usize, u32, u32)> {
        use std::os::fd::AsRawFd;
        if !self.options.import_dmabuf {
            return None;
        }
        let first = input.dmabuf_plane(0)?;
        let id = dma_heap::dmabuf_id(first.fd);
        let views = input.planes_visible().ok()?;
        let mut planes = [PlaneAt {
            same: true,
            offset: 0,
            stride: 0,
        }; 2];
        if views.len() > planes.len() {
            return None;
        }
        for (i, (view, at)) in views.iter().zip(planes.iter_mut()).enumerate() {
            let plane = input.dmabuf_plane(i)?;
            *at = PlaneAt {
                same: plane.fd.as_raw_fd() == first.fd.as_raw_fd()
                    || (id.is_some() && dma_heap::dmabuf_id(plane.fd) == id),
                offset: plane.offset,
                stride: view.stride(),
            };
        }
        let height = input.meta().format.resolution.height.get() as usize;
        let (hor, ver) = import_strides(input.meta().format.code, height, &planes[..views.len()])?;
        let size = dma_heap::dmabuf_size(first.fd).ok()? as usize;
        Some((first.fd.as_raw_fd(), size, hor, ver))
    }
}

impl Codec for RkMppEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        let format = input.meta().format;
        if format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: format.code,
            });
        }
        let (mpp_fmt, bpp) = mpp_format(format.code)
            .ok_or_else(|| mpp_err("input", format!("{} is not NV12 or YUYV", format.code)))?;
        let coding = mpp_coding(self.descriptor.output)
            .ok_or_else(|| mpp_err("output", format!("{}", self.descriptor.output)))?;
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let in_place = self.in_place(input);
        let (hor, ver) = in_place.map_or_else(
            || staging_strides(bpp, width, height),
            |(_, _, hor, ver)| (hor, ver),
        );
        let mut guard = self.session.lock();
        if guard
            .as_ref()
            .is_none_or(|s| s.key != (width, height, hor, ver))
        {
            *guard = None;
            *guard = Some(Session::open(
                coding,
                mpp_fmt,
                (width, height),
                (hor, ver),
                &self.options,
            )?);
        }
        let session = guard.as_mut().expect("opened above");
        if self.force_key.swap(false, Ordering::AcqRel) {
            let _ = session.control(ffi::ENC_SET_IDR_FRAME, ptr::null_mut());
        }
        let (buffer, imported) = match in_place {
            Some((fd, size, ..)) => (session.import(fd, size)?, true),
            None => {
                let (hor, ver) = (hor as usize, ver as usize);
                let size = if bpp == 1 {
                    hor * ver * 3 / 2
                } else {
                    hor * ver
                };
                let staging = session.staging(size)?;
                staging
                    .buf
                    .begin_cpu_access(Access::Write)
                    .map_err(|e| mpp_err("staging", e))?;
                let dst = staging.map.as_mut_slice();
                let planes = input.planes_visible().map_err(|e| mpp_err("input", e))?;
                let mut at = 0;
                for plane in planes.iter() {
                    for r in 0..plane.len() {
                        if let (Some(row), Some(out)) = (plane.row(r), dst.get_mut(at + r * hor..))
                        {
                            let row = row.data();
                            let n = row.len().min(out.len());
                            out[..n].copy_from_slice(&row[..n]);
                        }
                    }
                    at += hor * ver;
                }
                staging
                    .buf
                    .end_cpu_access(Access::Write)
                    .map_err(|e| mpp_err("staging", e))?;
                (staging.mpp, false)
            }
        };
        let (output, pool) = (self.descriptor.output, &self.pool);
        let inter = coding != ffi::CODING_MJPEG;
        let meta = input.meta();
        let result = session.encode(buffer, mpp_fmt, meta.timestamp as i64, |data, intra| {
            let mut buf = pool.lease_sized(data.len());
            buf.as_mut_slice()[..data.len()].copy_from_slice(data);
            let mut packet = FrameLease::single_plane(
                FrameMeta::new(
                    MediaFormat::new(output, meta.format.resolution, meta.format.color),
                    meta.timestamp,
                )
                .with_residency(FrameResidency::CompressedPacket),
                buf,
                data.len(),
                data.len(),
            );
            packet.meta_mut().delta = inter && !intra;
            packet
        });
        if imported {
            session.put_buffer(buffer);
        }
        result
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Release);
    }

    fn new_instance(&self) -> Option<Arc<dyn Codec>> {
        Some(Arc::new(Self::new(
            self.descriptor.input,
            self.descriptor.output,
            self.options.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(same: bool, offset: usize, stride: usize) -> PlaneAt {
        PlaneAt {
            same,
            offset,
            stride,
        }
    }

    #[test]
    fn nv12_in_one_dmabuf_is_read_in_place() {
        let nv12 = FourCc::NV12;
        // The ISP's NV12: chroma after 800 rows at a 1280-byte stride.
        let planes = [at(true, 0, 1280), at(true, 1280 * 800, 1280)];
        assert_eq!(import_strides(nv12, 800, &planes), Some((1280, 800)));
        // Chroma after padded rows: the vertical stride says so.
        let planes = [at(true, 0, 1280), at(true, 1280 * 816, 1280)];
        assert_eq!(import_strides(nv12, 800, &planes), Some((1280, 816)));
    }

    #[test]
    fn other_layouts_are_copied() {
        let nv12 = FourCc::NV12;
        for planes in [
            [at(false, 0, 1280), at(false, 1280 * 800, 1280)],
            [at(true, 64, 1280), at(true, 1280 * 800 + 64, 1280)],
            [at(true, 0, 1280), at(true, 1280 * 800 + 7, 1280)],
            [at(true, 0, 1280), at(true, 1280 * 400, 1280)],
            [at(true, 0, 1280), at(true, 1280 * 800, 1536)],
        ] {
            assert_eq!(import_strides(nv12, 800, &planes), None, "{planes:?}");
        }
        assert_eq!(
            import_strides(FourCc::YUYV, 800, &[at(true, 0, 2560)]),
            Some((2560, 800))
        );
        assert_eq!(
            import_strides(FourCc::RGB3, 800, &[at(true, 0, 3840)]),
            None
        );
    }

    #[test]
    fn staging_rows_and_heights_are_aligned_to_16() {
        assert_eq!(staging_strides(1, 1280, 800), (1280, 800));
        assert_eq!(staging_strides(1, 1000, 750), (1008, 752));
        assert_eq!(staging_strides(2, 1000, 750), (2016, 752));
    }

    #[test]
    fn formats_and_codings_map_to_mpp() {
        assert_eq!(mpp_format(FourCc::NV12), Some((ffi::FMT_YUV420SP, 1)));
        assert_eq!(mpp_format(FourCc::YUYV), Some((ffi::FMT_YUV422_YUYV, 2)));
        assert_eq!(mpp_format(FourCc::RGB3), None);
        assert_eq!(mpp_coding(FourCc::H264), Some(ffi::CODING_AVC));
        assert_eq!(mpp_coding(FourCc::H265), Some(ffi::CODING_HEVC));
        assert_eq!(mpp_coding(FourCc::MJPG), Some(ffi::CODING_MJPEG));
    }

    /// Without MPP (this PC), the encoder is not offered and a frame is an error, not a
    /// crash.
    #[test]
    fn without_mpp_nothing_is_offered() {
        if Path::new("/dev/mpp_service").exists() {
            return;
        }
        assert!(!RkMppEncoder::available(FourCc::NV12, FourCc::H264));
        let codec = RkMppEncoder::new(FourCc::NV12, FourCc::H264, RkMppOptions::default());
        let format = MediaFormat::new(
            FourCc::NV12,
            Resolution::new(64, 48).unwrap(),
            ColorSpace::Srgb,
        );
        let frame = FrameLease::allocate_host_owned(format, 1).unwrap();
        assert!(codec.process_ref(&frame).is_err());
    }
}

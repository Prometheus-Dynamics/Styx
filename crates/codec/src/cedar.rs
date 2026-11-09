//! H.264 encoding on the Allwinner A733's video engine through the vendor's CedarC libraries
//! (`libvencoder`, opened with `dlopen`; `/dev/cedar_dev_ve2`, which has no V4L2 interface):
//! NV12 in, Annex-B H.264 out ([`CedarEncoder`]). Written, untested on board.
//!
//! Each frame is copied into an input buffer the library allocates, and encoded before the
//! call returns. Encoding a dma-buf in place is not written. SPS and PPS go before every
//! keyframe.

mod ffi;

use std::ffi::{c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use styx_core::prelude::*;

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// The library pads frames to 16 rows and columns.
const ALIGN: u32 = 16;
/// Input buffers the library allocates: one, as each call encodes the frame it copied.
const INPUT_BUFFERS: u32 = 1;
/// What a padded row or column of each plane holds: black.
const LUMA_FILL: u8 = 16;
const CHROMA_FILL: u8 = 128;

/// How a CedarC encoder encodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CedarOptions {
    /// The target bit rate in bits per second (`None`: 1/8 bit per pixel per second).
    pub bitrate: Option<u32>,
    /// Frames from one keyframe to the next (`None`: two seconds).
    pub gop: Option<u32>,
    /// The frame rate the rate control plans for.
    pub fps: u32,
}

impl Default for CedarOptions {
    fn default() -> Self {
        Self {
            bitrate: None,
            gop: None,
            fps: 30,
        }
    }
}

fn cedar_err(context: &str, e: impl std::fmt::Display) -> CodecError {
    CodecError::Codec(format!("cedar {context}: {e}"))
}

fn check(context: &str, ret: c_int) -> Result<(), CodecError> {
    if ret == 0 {
        Ok(())
    } else {
        Err(cedar_err(context, format!("CedarC error {ret}")))
    }
}

/// The smallest H.264 level (as the headers number them) that holds this size and rate.
fn h264_level(width: u32, height: u32, fps: u32) -> c_int {
    let macroblocks = width.div_ceil(16) * height.div_ceil(16);
    match (macroblocks, macroblocks.saturating_mul(fps)) {
        (..=3_600, ..=108_000) => 31,
        (..=8_192, ..=245_760) => 40,
        (..=8_704, ..=522_240) => 42,
        _ => 51,
    }
}

/// Copies `rows` into `dst` at `stride`, filling the end of each row and the rows after them
/// up to `height` with `fill`.
fn copy_plane<'a>(
    rows: impl Iterator<Item = &'a [u8]>,
    dst: &mut [u8],
    stride: usize,
    height: usize,
    fill: u8,
) {
    let mut lines = dst.chunks_exact_mut(stride).take(height);
    // Rows first: zipping the other way round would lose a line when the rows run out.
    for (row, line) in rows.zip(lines.by_ref()) {
        let n = row.len().min(stride);
        line[..n].copy_from_slice(&row[..n]);
        line[n..].fill(fill);
    }
    for line in lines {
        line.fill(fill);
    }
}

/// An encoder set up for one frame size.
struct Session {
    cedar: &'static ffi::Cedar,
    memops: *mut ffi::MemOps,
    encoder: ffi::Encoder,
    inputs_allocated: bool,
    size: (u32, u32),
    /// SPS and PPS.
    headers: Box<[u8]>,
}

// SAFETY: the library's handles are used by one thread at a time, under the encoder's mutex.
unsafe impl Send for Session {}

impl Session {
    fn open(
        cedar: &'static ffi::Cedar,
        (width, height): (u32, u32),
        options: &CedarOptions,
    ) -> Result<Self, CodecError> {
        // SAFETY: returns the library's static table, or null.
        let memops = unsafe { (cedar.MemAdapterGetOpsS)() };
        // SAFETY: a table from `MemAdapterGetOpsS`, when not null.
        let (open, close) = unsafe { memops.as_ref() }
            .and_then(|ops| Some((ops.open?, ops.close?)))
            .ok_or_else(|| cedar_err("memory", "no memory adapter"))?;
        // SAFETY: the adapter's own function, closed in `Drop` or below.
        check("memory", unsafe { open() })?;
        // SAFETY: a codec the library knows.
        let encoder = unsafe { (cedar.VideoEncCreate)(ffi::CODEC_H264) };
        if encoder.is_null() {
            // SAFETY: opened above and not used after.
            unsafe { close() };
            return Err(cedar_err("create", "VideoEncCreate failed"));
        }
        let mut session = Self {
            cedar,
            memops,
            encoder,
            inputs_allocated: false,
            size: (width, height),
            headers: Box::default(),
        };
        session.configure(options)?;
        Ok(session)
    }

    fn set(&self, context: &str, index: c_int, param: *mut c_void) -> Result<(), CodecError> {
        // SAFETY: this encoder and the parameter type `index` takes.
        check(context, unsafe {
            (self.cedar.VideoEncSetParameter)(self.encoder, index, param)
        })
    }

    fn configure(&mut self, options: &CedarOptions) -> Result<(), CodecError> {
        let (width, height) = self.size;
        let fps = options.fps.max(1);
        let bitrate = options.bitrate.unwrap_or(width * height * fps / 8);
        // SAFETY: an all-zero `VencH264Param` is the library's "unset".
        let mut h264: ffi::H264Param = unsafe { std::mem::zeroed() };
        h264.profile = ffi::PROFILE_BASELINE;
        h264.level = h264_level(width, height, fps);
        h264.min_qp = 10;
        h264.max_qp = 40;
        h264.min_p_qp = 10;
        h264.max_p_qp = 50;
        h264.init_qp = 30;
        h264.framerate = fps as c_int;
        h264.src_framerate = fps as c_int;
        h264.bitrate = bitrate as c_int;
        h264.max_key_interval = options.gop.unwrap_or(fps * 2) as c_int;
        h264.coding_mode = ffi::FRAME_CODING;
        h264.rc_mode = ffi::RC_CBR;
        let (mut filter, mut pskip): (c_int, c_int) = (0, 1);
        self.set("H.264", ffi::PARAM_H264, (&raw mut h264).cast())?;
        self.set("filter", ffi::PARAM_IFILTER, (&raw mut filter).cast())?;
        self.set("skip", ffi::PARAM_PSKIP, (&raw mut pskip).cast())?;

        let (stored_w, stored_h) = stored_size(width, height);
        // SAFETY: all zero is the library's "unset" for the fields not named.
        let mut base: ffi::BaseConfig = unsafe { std::mem::zeroed() };
        base.input_width = width;
        base.input_height = height;
        base.dst_width = width;
        base.dst_height = height;
        base.stride = stored_w;
        base.input_format = ffi::PIXEL_YUV420SP;
        base.memops = self.memops;
        let c = self.cedar;
        // SAFETY: this encoder and a config living across the call.
        check("init", unsafe { (c.VideoEncInit)(self.encoder, &mut base) })?;
        let mut alloc = ffi::AllocParam {
            count: INPUT_BUFFERS,
            size_y: stored_w * stored_h,
            size_c: stored_w * stored_h / 2,
        };
        // SAFETY: an initialised encoder and parameters living across the call.
        check("input buffers", unsafe {
            (c.AllocInputBuffer)(self.encoder, &mut alloc)
        })?;
        self.inputs_allocated = true;
        let mut headers = ffi::HeaderData {
            data: ptr::null_mut(),
            len: 0,
        };
        // SAFETY: an initialised encoder, and the type this index fills.
        check("headers", unsafe {
            (c.VideoEncGetParameter)(
                self.encoder,
                ffi::PARAM_H264_HEADERS,
                (&raw mut headers).cast(),
            )
        })?;
        if headers.data.is_null() || headers.len <= 0 {
            return Err(cedar_err("headers", "no SPS or PPS"));
        }
        // SAFETY: the library's buffer of `len` bytes, copied before any other call.
        self.headers =
            unsafe { std::slice::from_raw_parts(headers.data, headers.len as usize) }.into();
        Ok(())
    }

    /// Encodes `input`, and calls `take` with the packet's parts and whether it is a keyframe.
    fn encode<T>(
        &mut self,
        input: &FrameLease,
        take: impl FnOnce(&[u8], [&[u8]; 3], bool) -> T,
    ) -> Result<T, CodecError> {
        let c = self.cedar;
        let (width, height) = self.size;
        let (stored_w, stored_h) = stored_size(width, height);
        let stride = stored_w as usize;
        // SAFETY: the library fills it; all zero is valid for its integers and pointers.
        let mut buffer: ffi::InputBuffer = unsafe { std::mem::zeroed() };
        // SAFETY: this encoder and an out-parameter.
        check("input", unsafe {
            (c.GetOneAllocInputBuffer)(self.encoder, &mut buffer)
        })?;
        let filled = self.fill(input, &mut buffer, stride, stored_h as usize);
        // SAFETY: the buffer the library gave, flushed then added once.
        let added = filled.and_then(|()| {
            check("flush", unsafe {
                (c.FlushCacheAllocInputBuffer)(self.encoder, &mut buffer)
            })?;
            check("add", unsafe {
                (c.AddOneInputBuffer)(self.encoder, &mut buffer)
            })
        });
        if let Err(e) = added {
            // SAFETY: the buffer the library gave, not added.
            unsafe { (c.ReturnOneAllocInputBuffer)(self.encoder, &mut buffer) };
            return Err(e);
        }
        // SAFETY: this encoder, holding the frame added above.
        check("encode", unsafe { (c.VideoEncodeOneFrame)(self.encoder) })?;
        // SAFETY: zeroed as above; the library fills it with the buffer it finished.
        let mut used: ffi::InputBuffer = unsafe { std::mem::zeroed() };
        // SAFETY: this encoder and out-parameters; the used buffer goes back once.
        unsafe {
            check("used", (c.AlreadyUsedInputBuffer)(self.encoder, &mut used))?;
            check(
                "return",
                (c.ReturnOneAllocInputBuffer)(self.encoder, &mut used),
            )?;
            if (c.ValidBitstreamFrameNum)(self.encoder) <= 0 {
                return Err(CodecError::Backpressure);
            }
        }
        // SAFETY: zeroed as above; the library fills it.
        let mut output: ffi::OutputBuffer = unsafe { std::mem::zeroed() };
        // SAFETY: this encoder and an out-parameter.
        check("output", unsafe {
            (c.GetOneBitstreamFrame)(self.encoder, &mut output)
        })?;
        let part = |data: *mut u8, size: u32| -> &[u8] {
            if data.is_null() || size == 0 {
                &[]
            } else {
                // SAFETY: the library's bytes, valid until the frame is freed below.
                unsafe { std::slice::from_raw_parts(data, size as usize) }
            }
        };
        let parts = [
            part(output.data0, output.size0),
            part(output.data1, output.size1),
            part(output.data2, output.size2),
        ];
        let key = output.flag & ffi::FLAG_KEYFRAME != 0;
        let packet = take(if key { &self.headers } else { &[] }, parts, key);
        // SAFETY: the frame from `GetOneBitstreamFrame`, freed once; its bytes are not used after.
        check("free", unsafe {
            (c.FreeOneBitStreamFrame)(self.encoder, &mut output)
        })?;
        Ok(packet)
    }

    /// Copies the frame into the library's input buffer and describes it.
    fn fill(
        &self,
        input: &FrameLease,
        buffer: &mut ffi::InputBuffer,
        stride: usize,
        stored_h: usize,
    ) -> Result<(), CodecError> {
        if buffer.y.is_null() || buffer.c.is_null() {
            return Err(cedar_err("input", "no input buffer"));
        }
        let planes = input.planes_visible().map_err(|e| cedar_err("input", e))?;
        let [y, uv] = planes.as_slice() else {
            return Err(cedar_err("input", "NV12 has two planes"));
        };
        // SAFETY: the library allocated `stride * stored_h` bytes of luma and half of chroma
        // for this buffer (`AllocInputBuffer`), and hands it to one user at a time.
        let (dst_y, dst_c) = unsafe {
            (
                std::slice::from_raw_parts_mut(buffer.y, stride * stored_h),
                std::slice::from_raw_parts_mut(buffer.c, stride * stored_h / 2),
            )
        };
        copy_plane(
            (0..y.len()).filter_map(|r| y.row(r).map(|row| row.data())),
            dst_y,
            stride,
            stored_h,
            LUMA_FILL,
        );
        copy_plane(
            (0..uv.len()).filter_map(|r| uv.row(r).map(|row| row.data())),
            dst_c,
            stride,
            stored_h / 2,
            CHROMA_FILL,
        );
        let (width, height) = self.size;
        buffer.pts = (input.meta().timestamp / 1_000) as i64;
        buffer.flag = 0;
        buffer.width = width as c_int;
        buffer.height = height as c_int;
        buffer.align = ALIGN as c_int;
        buffer.enable_crop = 0;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let c = self.cedar;
        // SAFETY: this session's encoder and adapter, released once, in the library's order.
        unsafe {
            if self.inputs_allocated {
                (c.ReleaseAllocInputBuffer)(self.encoder);
            }
            (c.VideoEncDestroy)(self.encoder);
            if let Some(close) = self.memops.as_ref().and_then(|ops| ops.close) {
                close();
            }
        }
    }
}

/// The size of the library's input buffers for a frame.
fn stored_size(width: u32, height: u32) -> (u32, u32) {
    (
        width.next_multiple_of(ALIGN),
        height.next_multiple_of(ALIGN),
    )
}

/// A hardware H.264 encoder on the A733's video engine. Written, untested on board.
pub struct CedarEncoder {
    descriptor: CodecDescriptor,
    options: CedarOptions,
    session: Mutex<Option<Session>>,
    force_key: AtomicBool,
    pool: BufferPool,
}

impl CedarEncoder {
    /// An H.264 encoder of NV12 frames. CedarC opens on the first frame.
    pub fn new(options: CedarOptions) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input: FourCc::NV12,
                output: FourCc::H264,
                name: "cedar",
                impl_name: "cedar",
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

    /// Whether CedarC can encode here: an aarch64 system (the layouts are for it) with the
    /// video engine's device and the library.
    pub fn available() -> bool {
        cfg!(target_arch = "aarch64")
            && Path::new("/dev/cedar_dev_ve2").exists()
            && ffi::cedar().is_ok()
    }
}

impl Codec for CedarEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        let format = input.meta().format;
        if format.code != FourCc::NV12 {
            return Err(CodecError::FormatMismatch {
                expected: FourCc::NV12,
                actual: format.code,
            });
        }
        let size = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        if !size.0.is_multiple_of(2) || !size.1.is_multiple_of(2) {
            return Err(cedar_err("input", "width and height must be even"));
        }
        let cedar = ffi::cedar().map_err(|e| cedar_err("library", e))?;
        let mut guard = self.session.lock();
        if guard.as_ref().is_none_or(|s| s.size != size) {
            *guard = None;
            *guard = Some(Session::open(cedar, size, &self.options)?);
        }
        let session = guard.as_mut().expect("opened above");
        if self.force_key.swap(false, Ordering::AcqRel) {
            let mut force: c_int = 1;
            session.set(
                "keyframe",
                ffi::PARAM_FORCE_KEY_FRAME,
                (&raw mut force).cast(),
            )?;
        }
        let (pool, meta) = (&self.pool, input.meta());
        let result = session.encode(input, |headers, parts, key| {
            let len = headers.len() + parts.iter().map(|p| p.len()).sum::<usize>();
            let mut buf = pool.lease_sized(len);
            let mut at = 0;
            for part in [headers, parts[0], parts[1], parts[2]] {
                buf.as_mut_slice()[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
            let mut packet = FrameLease::single_plane(
                FrameMeta::new(
                    MediaFormat::new(FourCc::H264, meta.format.resolution, meta.format.color),
                    meta.timestamp,
                )
                .with_residency(FrameResidency::CompressedPacket),
                buf,
                len,
                len,
            );
            packet.meta_mut().delta = !key;
            packet
        });
        // The next frame starts from a fresh encoder, should this one have failed.
        if result.is_err() {
            *guard = None;
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
        Some(Arc::new(Self::new(self.options.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_hold_the_size_and_rate() {
        assert_eq!(h264_level(1280, 720, 30), 31);
        assert_eq!(h264_level(1920, 1080, 30), 40);
        assert_eq!(h264_level(1920, 1080, 60), 42);
        assert_eq!(h264_level(3840, 2160, 30), 51);
    }

    #[test]
    fn planes_are_padded_to_the_stored_size() {
        let (row0, row1) = ([1u8, 2], [3u8, 4]);
        let mut dst = [0u8; 12];
        copy_plane([&row0[..], &row1[..]].into_iter(), &mut dst, 4, 3, 9);
        assert_eq!(dst, [1, 2, 9, 9, 3, 4, 9, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn stored_sizes_are_aligned_to_16() {
        assert_eq!(stored_size(1920, 1080), (1920, 1088));
        assert_eq!(stored_size(640, 360), (640, 368));
    }

    #[test]
    fn without_the_library_nothing_is_offered() {
        if !Path::new("/dev/cedar_dev_ve2").exists() {
            assert!(!CedarEncoder::available());
        }
    }
}

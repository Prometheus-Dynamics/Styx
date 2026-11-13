//! The FFmpeg decoder and encoder: frames in and out as leases, through the runtime-loaded
//! libraries.
//!
//! One FFmpeg session is one libavcodec context plus the frame and packet it reuses. Frames go
//! into FFmpeg by pointer where FFmpeg accepts that (the input lease's planes are read, and
//! copied into the frame FFmpeg allocated once, counted in [`COPY_TO_FFMPEG`]); decoded frames
//! are copied into the caller's output lease (counted in [`COPY_FROM_FFMPEG`]). FFmpeg's own
//! buffers stay FFmpeg's. A DRM PRIME frame from a hardware decoder is not copied:
//! [`FfmpegDecoder::decode_prime`] hands it on as a lease over its dma-buf (see `prime`).

use std::ffi::{CString, c_int, c_void};
use std::fmt;
use std::sync::Arc;

use styx_codec_api::{Backend, Codec, CodecDescriptor, CodecError, CodecKind, Decoder, Encoder};
use styx_format::{FourCc, plane_geometry};
use styx_frame::{CopyCounter, FrameEntry, FrameLease, PlaneLayout};

use crate::api::{Api, error_text};
use crate::prime::{Decoded, PrimeSink};
use crate::sys::{
    ERR_EAGAIN, FRAME_DATA, FRAME_FORMAT, FRAME_HEIGHT, FRAME_LINESIZE, FRAME_WIDTH, PACKET_DATA,
    PACKET_PTS, PACKET_SIZE, PIX_GRAY8, PIX_NV12, PIX_YUV420P, PIX_YUVJ420P, PIX_YUYV422, read_int,
    read_ptr, write_i64, write_int,
};

/// Bytes copied from a Styx lease into FFmpeg's frames or packets.
pub static COPY_TO_FFMPEG: CopyCounter = CopyCounter::new("styx-ffmpeg: lease into FFmpeg");
/// Bytes copied from FFmpeg's frames or packets into a Styx lease.
pub static COPY_FROM_FFMPEG: CopyCounter = CopyCounter::new("styx-ffmpeg: FFmpeg into lease");

/// Maps a Styx raw format to its FFmpeg pixel format and name for the `pix_fmt` option.
fn to_pix(code: FourCc) -> Option<(c_int, &'static str)> {
    Some(match code {
        FourCc::I420 => (PIX_YUV420P, "yuv420p"),
        FourCc::NV12 => (PIX_NV12, "nv12"),
        FourCc::YUYV => (PIX_YUYV422, "yuyv422"),
        FourCc::GREY => (PIX_GRAY8, "gray"),
        _ => return None,
    })
}

/// Maps an FFmpeg pixel format a decoder produced to a Styx raw format. JPEG's full-range
/// YUV 4:2:0 is the same bytes as I420; its range is not carried by the frame format.
fn from_pix(pix: c_int) -> Option<FourCc> {
    Some(match pix {
        PIX_YUV420P | PIX_YUVJ420P => FourCc::I420,
        PIX_NV12 => FourCc::NV12,
        PIX_YUYV422 => FourCc::YUYV,
        PIX_GRAY8 => FourCc::GREY,
        _ => return None,
    })
}

/// One libavcodec context with the frame and packet it reuses. Freed on drop.
struct Session {
    api: Arc<Api>,
    ctx: *mut c_void,
    frame: *mut c_void,
    packet: *mut c_void,
}

// SAFETY: a session owns its context, frame and packet exclusively, and FFmpeg contexts may be
// used from another thread as long as one thread at a time uses them (Codec requires only Send).
unsafe impl Send for Session {}

impl Session {
    /// Opens the FFmpeg codec `name` as a decoder or encoder, with string options set first.
    fn open(
        api: Arc<Api>,
        name: &str,
        encoder: bool,
        options: &[(&str, &str)],
    ) -> Result<Self, CodecError> {
        let cname = CString::new(name).map_err(|e| CodecError::Unavailable(e.to_string()))?;
        // SAFETY: `cname` is a valid NUL-terminated string for the call.
        let codec = unsafe {
            if encoder {
                (api.codec.avcodec_find_encoder_by_name)(cname.as_ptr())
            } else {
                (api.codec.avcodec_find_decoder_by_name)(cname.as_ptr())
            }
        };
        if codec.is_null() {
            return Err(CodecError::Unavailable(format!(
                "libavcodec has no {name} {}",
                if encoder { "encoder" } else { "decoder" }
            )));
        }
        // SAFETY: `codec` is a non-null codec from libavcodec, which lives for the process.
        let ctx = unsafe { (api.codec.avcodec_alloc_context3)(codec) };
        if ctx.is_null() {
            return Err(CodecError::Failed(
                "avcodec_alloc_context3 returned null".into(),
            ));
        }
        let mut session = Self {
            api,
            ctx,
            frame: std::ptr::null_mut(),
            packet: std::ptr::null_mut(),
        };
        for (key, value) in options {
            session.set_option(key, value)?;
        }
        // SAFETY: `ctx` is a context allocated for `codec`, not yet open; the dictionary is null.
        let ret = unsafe { (session.api.codec.avcodec_open2)(ctx, codec, std::ptr::null_mut()) };
        if ret < 0 {
            return Err(CodecError::Unavailable(format!(
                "{name} did not open: {}",
                error_text(&session.api, ret)
            )));
        }
        // SAFETY: both allocators take no arguments and return an owned pointer or null.
        session.frame = unsafe { (session.api.util.av_frame_alloc)() };
        // SAFETY: as above.
        session.packet = unsafe { (session.api.codec.av_packet_alloc)() };
        if session.frame.is_null() || session.packet.is_null() {
            return Err(CodecError::Failed(
                "out of memory for an FFmpeg frame or packet".into(),
            ));
        }
        Ok(session)
    }

    fn set_option(&mut self, key: &str, value: &str) -> Result<(), CodecError> {
        let ckey = CString::new(key).map_err(|e| CodecError::Failed(e.to_string()))?;
        let cvalue = CString::new(value).map_err(|e| CodecError::Failed(e.to_string()))?;
        // SAFETY: the context is a live AVClass-headed object; both strings are NUL-terminated and
        // live for the call.
        let ret =
            unsafe { (self.api.util.av_opt_set)(self.ctx, ckey.as_ptr(), cvalue.as_ptr(), 0) };
        if ret < 0 {
            return Err(CodecError::Failed(format!(
                "option {key}={value}: {}",
                error_text(&self.api, ret)
            )));
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: each pointer is owned by this session and freed once, here; the free functions
        // take the address of the pointer and null it.
        unsafe {
            (self.api.codec.av_packet_free)(&mut self.packet);
            (self.api.util.av_frame_free)(&mut self.frame);
            (self.api.codec.avcodec_free_context)(&mut self.ctx);
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("open", &!self.ctx.is_null())
            .finish()
    }
}

/// Copies a lease's planes (`geometry` rows of each plane) into an FFmpeg frame's planes.
///
/// # Safety
///
/// `frame` must be a frame whose buffers were allocated by FFmpeg for this geometry: each plane
/// `i` has at least `rows` rows of `linesize[i]` bytes from `data[i]`.
unsafe fn copy_into_frame<M>(
    input: &FrameLease<M>,
    frame: *mut c_void,
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<(), CodecError> {
    let geometry =
        plane_geometry(code, width, height).map_err(|e| CodecError::Rejected(e.to_string()))?;
    let src = input
        .read()
        .ok_or_else(|| CodecError::Rejected("input frame is not mapped for reading".into()))?;
    let mut copied = 0usize;
    let mut i = 0;
    while let Some(shape) = geometry.get(i) {
        let plane = src
            .plane(i)
            .ok_or_else(|| CodecError::Rejected(format!("input plane {i} missing")))?;
        // SAFETY: the frame's plane pointers are FFmpeg's buffers, sized for the rows by the
        // caller's contract; `shape` gives the rows and bytes per row from the same geometry.
        let (dst, stride) = unsafe {
            (
                read_ptr(frame, FRAME_DATA + 8 * i),
                read_int(frame, FRAME_LINESIZE + 4 * i) as usize,
            )
        };
        if dst.is_null() {
            return Err(CodecError::Failed(format!(
                "FFmpeg frame plane {i} has no buffer"
            )));
        }
        for row in 0..shape.rows {
            let from = plane
                .row_bytes(row, shape.row_bytes)
                .ok_or_else(|| CodecError::Rejected(format!("input plane {i} is too short")))?;
            // SAFETY: `row < rows` and `row_bytes <= stride` for the geometry FFmpeg allocated,
            // so the destination row lies inside the buffer; the source slice has `row_bytes`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    from.as_ptr(),
                    dst.add(row * stride),
                    shape.row_bytes,
                );
            }
        }
        copied += shape.row_bytes * shape.rows;
        i += 1;
    }
    COPY_TO_FFMPEG.record(copied);
    Ok(())
}

/// Copies a decoded FFmpeg frame's planes into the caller's output lease.
///
/// # Safety
///
/// `frame` must hold a decoded frame of `code` at `width` x `height`, with its plane pointers and
/// line sizes valid for the geometry of that format.
unsafe fn copy_out_of_frame<M>(
    frame: *const c_void,
    output: &mut FrameLease<M>,
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<(), CodecError> {
    let geometry =
        plane_geometry(code, width, height).map_err(|e| CodecError::Rejected(e.to_string()))?;
    let mut out = output
        .write()
        .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
    let mut copied = 0usize;
    let mut i = 0;
    while let Some(shape) = geometry.get(i) {
        // SAFETY: the decoded frame's planes are valid for their rows (the caller's contract);
        // `linesize` is the stride FFmpeg wrote for this plane.
        let (src, stride) = unsafe {
            (
                read_ptr(frame, FRAME_DATA + 8 * i) as *const u8,
                read_int(frame, FRAME_LINESIZE + 4 * i) as usize,
            )
        };
        if src.is_null() {
            return Err(CodecError::Failed(format!(
                "decoded frame plane {i} is null"
            )));
        }
        let mut to = out
            .plane(i)
            .ok_or_else(|| CodecError::Rejected(format!("output plane {i} missing")))?;
        for row in 0..shape.rows {
            let dst = to
                .row_bytes_mut(row, shape.row_bytes)
                .ok_or_else(|| CodecError::Rejected(format!("output plane {i} is too short")))?;
            // SAFETY: `row < rows` and `row_bytes <= stride`, so the source row lies inside the
            // decoded plane; `dst` has exactly `row_bytes` bytes.
            let from =
                unsafe { std::slice::from_raw_parts(src.add(row * stride), shape.row_bytes) };
            dst.copy_from_slice(from);
        }
        copied += shape.row_bytes * shape.rows;
        i += 1;
    }
    COPY_FROM_FFMPEG.record(copied);
    Ok(())
}

/// A decoder for one compressed format, producing one raw format per packet.
pub struct FfmpegDecoder {
    descriptor: CodecDescriptor,
    session: Session,
}

impl FfmpegDecoder {
    pub(crate) fn open(api: Arc<Api>, descriptor: CodecDescriptor) -> Result<Self, CodecError> {
        let session = Session::open(api, descriptor.name, false, &[])?;
        Ok(Self {
            descriptor,
            session,
        })
    }
}

impl fmt::Debug for FfmpegDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfmpegDecoder")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl<M> Codec<M> for FfmpegDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Decoder<M> for FfmpegDecoder {
    fn decode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        self.send_receive(input)?;
        let frame = self.session.frame;
        let result = self.take_frame(output, frame, input);
        // SAFETY: the frame is this session's; unref releases what the decoder gave it.
        unsafe { (self.session.api.util.av_frame_unref)(frame) };
        result
    }
}

impl FfmpegDecoder {
    /// Sends `input`'s bitstream as one packet and receives the frame it produced into the
    /// session's frame, which the caller then takes (or unrefs).
    fn send_receive<M>(&mut self, input: &FrameLease<M>) -> Result<(), CodecError> {
        let api = self.session.api.clone();
        let (ctx, frame, packet) = (self.session.ctx, self.session.frame, self.session.packet);

        let src = input
            .read()
            .ok_or_else(|| CodecError::Rejected("input is not mapped for reading".into()))?;
        let bytes = src
            .plane(0)
            .ok_or_else(|| CodecError::Rejected("input has no bitstream plane".into()))?
            .data;
        if bytes.is_empty() {
            return Err(CodecError::Rejected("empty bitstream".into()));
        }
        let len = c_int::try_from(bytes.len())
            .map_err(|_| CodecError::Rejected("bitstream larger than FFmpeg allows".into()))?;
        // SAFETY: `packet` is this session's packet; unref drops its previous buffer.
        unsafe { (api.codec.av_packet_unref)(packet) };
        // SAFETY: `packet` is unreferenced; av_new_packet allocates `len` bytes for it.
        if unsafe { (api.codec.av_new_packet)(packet, len) } < 0 {
            return Err(CodecError::Failed("av_new_packet failed".into()));
        }
        // The bitstream is copied into the packet FFmpeg owns: a counted copy at this boundary.
        // SAFETY: the packet was just given `len` bytes at its data pointer, and `bytes` has
        // `len` bytes in the input lease; they do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                read_ptr(packet, PACKET_DATA),
                bytes.len(),
            );
            write_i64(packet, PACKET_PTS, input.timestamp_ns() as i64);
        }
        COPY_TO_FFMPEG.record(bytes.len());
        // SAFETY: `ctx` is an open decoder and `packet` is a valid packet for it.
        let sent = unsafe { (api.codec.avcodec_send_packet)(ctx, packet) };
        // SAFETY: the packet is no longer needed after sending; unref releases its buffer.
        unsafe { (api.codec.av_packet_unref)(packet) };
        drop(src);
        if sent < 0 {
            return Err(CodecError::Rejected(error_text(&api, sent)));
        }

        // SAFETY: `ctx` is open and `frame` is this session's frame.
        let got = unsafe { (api.codec.avcodec_receive_frame)(ctx, frame) };
        if got == ERR_EAGAIN {
            return Err(CodecError::Failed(
                "decoder has no frame for this packet yet".into(),
            ));
        }
        if got < 0 {
            return Err(CodecError::Failed(error_text(&api, got)));
        }
        Ok(())
    }
}

impl FfmpegDecoder {
    fn take_frame<M>(
        &self,
        output: &mut FrameLease<M>,
        frame: *mut c_void,
        input: &FrameLease<M>,
    ) -> Result<(), CodecError> {
        // SAFETY: `frame` holds the frame just received, so its size and format fields are set.
        let (width, height, pix) = unsafe {
            (
                read_int(frame, FRAME_WIDTH) as u32,
                read_int(frame, FRAME_HEIGHT) as u32,
                read_int(frame, FRAME_FORMAT),
            )
        };
        let code = from_pix(pix).ok_or_else(|| {
            CodecError::Failed(format!(
                "FFmpeg produced pixel format {pix}, which Styx does not map"
            ))
        })?;
        if code != self.descriptor.output {
            return Err(CodecError::Failed(format!(
                "decoder produced {code}, the descriptor says {}",
                self.descriptor.output
            )));
        }
        let format = *output.format();
        if format.code != code
            || format.resolution.width.get() != width
            || format.resolution.height.get() != height
        {
            return Err(CodecError::Rejected(format!(
                "output is not {code} {width}x{height}"
            )));
        }
        // SAFETY: the frame's planes are valid for this geometry, per the decoder's output.
        unsafe { copy_out_of_frame(frame, output, code, width, height)? };
        // The timestamp is the input's: one packet in, one frame out (see `sys`).
        let entry = FrameEntry::new(
            input.id(),
            input.timestamp_ns(),
            *output.format(),
            *output.layouts(),
        );
        output
            .set_entry(entry)
            .map_err(|e| CodecError::Failed(e.to_string()))
    }
}

impl FfmpegDecoder {
    /// A sink for this decoder's DRM PRIME frames: `payload` bytes per frame at most, room for
    /// `capacity` dma-bufs, and (when `cpu_access`) read-only CPU mappings of them.
    ///
    /// `capacity` must cover the frames the decoder keeps in flight (its surface pool) plus those
    /// consumers hold: a frame whose dma-buf is still held is dropped, and the decoder's own pool
    /// runs dry if the sink keeps too many slots busy.
    pub fn prime_sink<M: Default>(
        &self,
        payload: usize,
        capacity: usize,
        cpu_access: bool,
    ) -> Result<PrimeSink<M>, CodecError> {
        PrimeSink::new(self.session.api.clone(), payload, capacity, cpu_access)
    }

    /// Decodes `input` into a DRM PRIME frame and hands it on as a lease from `sink`, without
    /// copying the frame. The decoder supplies the lease; [`Decoder::decode`] takes a lease the
    /// caller acquired instead.
    ///
    /// Errors when the decoder's frame is not DRM PRIME (a software decoder, or a frame the
    /// descriptor cannot describe as one object). The frame is then dropped, and nothing is held.
    pub fn decode_prime<M>(
        &mut self,
        input: &FrameLease<M>,
        sink: &mut PrimeSink<M>,
    ) -> Result<Decoded<M>, CodecError> {
        self.send_receive(input)?;
        let frame = self.session.frame;
        let result = sink.accept(
            frame,
            self.descriptor.output,
            input.id(),
            input.timestamp_ns(),
        );
        // SAFETY: the frame is this session's. When the sink took it the frame is already empty
        // and this is a no-op; otherwise it releases what the decoder gave.
        unsafe { (self.session.api.util.av_frame_unref)(frame) };
        result
    }
}

/// An encoder for one raw format into one compressed format. Opened at the first frame's size.
pub struct FfmpegEncoder {
    descriptor: CodecDescriptor,
    api: Arc<Api>,
    session: Option<(Session, (u32, u32))>,
}

impl FfmpegEncoder {
    pub(crate) fn new(api: Arc<Api>, descriptor: CodecDescriptor) -> Self {
        Self {
            descriptor,
            api,
            session: None,
        }
    }
}

impl fmt::Debug for FfmpegEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfmpegEncoder")
            .field("descriptor", &self.descriptor)
            .field("open", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl<M> Codec<M> for FfmpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Encoder<M> for FfmpegEncoder {
    fn encode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let format = *input.format();
        let code = format.code;
        if code != self.descriptor.input {
            return Err(CodecError::Rejected(format!(
                "encoder takes {}, got {code}",
                self.descriptor.input
            )));
        }
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        // FFmpeg's MJPEG encoder takes only full-range YUV 4:2:0 (`yuvj420p`) unless strict
        // standards compliance is relaxed. Styx's I420 bytes are the same layout; the range is
        // taken as full, not converted.
        let (pix, pix_name) = if code == FourCc::I420 {
            (PIX_YUVJ420P, "yuvj420p")
        } else {
            to_pix(code)
                .ok_or_else(|| CodecError::Rejected(format!("FFmpeg does not take {code} here")))?
        };
        if self.session.is_none() {
            let size = format!("{width}x{height}");
            let session = Session::open(
                self.api.clone(),
                self.descriptor.name,
                true,
                &[
                    ("video_size", size.as_str()),
                    ("pixel_format", pix_name),
                    ("time_base", "1/1000000000"),
                ],
            )?;
            self.session = Some((session, (width, height)));
        }
        let (session, opened) = self.session.as_mut().expect("opened above");
        if *opened != (width, height) {
            return Err(CodecError::Rejected(format!(
                "encoder is open at {}x{}, the frame is {width}x{height}",
                opened.0, opened.1
            )));
        }
        let api = session.api.clone();
        let (ctx, frame, packet) = (session.ctx, session.frame, session.packet);

        // SAFETY: `frame` is this session's; its geometry fields are written before FFmpeg sizes
        // its buffers from them.
        unsafe {
            write_int(frame, FRAME_WIDTH, width as c_int);
            write_int(frame, FRAME_HEIGHT, height as c_int);
            write_int(frame, FRAME_FORMAT, pix);
        }
        // SAFETY: `frame` is this session's. The first frame gets buffers from FFmpeg; later
        // frames reuse them when FFmpeg holds no other reference.
        let ret = unsafe {
            if read_ptr(frame, FRAME_DATA).is_null() {
                (api.util.av_frame_get_buffer)(frame, 0)
            } else {
                (api.util.av_frame_make_writable)(frame)
            }
        };
        if ret < 0 {
            return Err(CodecError::Failed(error_text(&api, ret)));
        }
        // SAFETY: `frame` has its buffers for this geometry (sized above), and the planes are
        // written from the input lease's rows.
        unsafe { copy_into_frame(input, frame, code, width, height)? };
        // SAFETY: `ctx` is open and `frame` holds the frame to encode.
        let sent = unsafe { (api.codec.avcodec_send_frame)(ctx, frame) };
        if sent < 0 {
            return Err(CodecError::Failed(error_text(&api, sent)));
        }
        // SAFETY: `packet` is this session's; unref makes it reusable for the output.
        unsafe { (api.codec.av_packet_unref)(packet) };
        // SAFETY: `ctx` is open and `packet` is this session's packet.
        let got = unsafe { (api.codec.avcodec_receive_packet)(ctx, packet) };
        if got == ERR_EAGAIN {
            return Err(CodecError::Failed(
                "encoder has no packet for this frame yet".into(),
            ));
        }
        if got < 0 {
            return Err(CodecError::Failed(error_text(&api, got)));
        }
        // SAFETY: the packet is valid until unref; its size and data are set by the encoder.
        let (size, data) = unsafe {
            (
                read_int(packet, PACKET_SIZE).max(0) as usize,
                read_ptr(packet, PACKET_DATA) as *const u8,
            )
        };
        let result = (|| {
            let mut out = output
                .write()
                .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
            let bytes = out.bytes();
            if bytes.len() < size {
                return Err(CodecError::Rejected(format!(
                    "output buffer holds {} bytes, the packet has {size}",
                    bytes.len()
                )));
            }
            // SAFETY: the packet holds `size` bytes at `data`, and `bytes` has at least `size`
            // bytes; the two do not overlap (FFmpeg's buffer and the lease's buffer).
            unsafe { std::ptr::copy_nonoverlapping(data, bytes.as_mut_ptr(), size) };
            Ok(())
        })();
        // SAFETY: the packet's bytes have been copied (or are not needed); unref frees them.
        unsafe { (api.codec.av_packet_unref)(packet) };
        result?;
        COPY_FROM_FFMPEG.record(size);

        let layouts = packet_layout(size)?;
        output
            .set_entry(FrameEntry::new(
                input.id(),
                input.timestamp_ns(),
                *output.format(),
                layouts,
            ))
            .map_err(|e| CodecError::Failed(e.to_string()))
    }
}

/// The one-plane layout of a compressed packet of `size` bytes.
pub(crate) fn packet_layout(size: usize) -> Result<styx_frame::Layouts, CodecError> {
    styx_frame::Layouts::new(&[PlaneLayout::new(0, 0, size)])
        .ok_or_else(|| CodecError::Failed("packet layout".into()))
}

/// The descriptor for a built-in FFmpeg codec the registry offers.
pub(crate) fn descriptor(
    kind: CodecKind,
    input: FourCc,
    output: FourCc,
    name: &'static str,
) -> CodecDescriptor {
    CodecDescriptor {
        kind,
        input,
        output,
        backend: Backend::Ffmpeg,
        name,
    }
}

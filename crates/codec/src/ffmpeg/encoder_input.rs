//! How input frames reach FFmpeg: in place when possible, otherwise copied into a staging frame.

use std::any::Any;
use std::sync::Arc;

use styx_core::prelude::*;

use crate::CodecError;
use crate::ffmpeg::ff::{frame::Video as FfFrame, util::format::pixel::Pixel as PixelFormat};

pub(super) fn alloc_video_frame(
    fmt: PixelFormat,
    width: u32,
    height: u32,
) -> Result<FfFrame, CodecError> {
    let mut frame = FfFrame::empty();
    frame.set_format(fmt);
    frame.set_width(width);
    frame.set_height(height);
    unsafe {
        frame.alloc(fmt, width, height);
    }
    Ok(frame)
}

pub(super) fn write_input_frame(
    dst: &mut FfFrame,
    fmt: PixelFormat,
    frame: &FrameLease,
) -> Result<(), CodecError> {
    let meta = frame.meta();
    let width = meta.format.resolution.width.get();
    let height = meta.format.resolution.height.get();
    if dst.width() != width || dst.height() != height {
        return Err(CodecError::Codec(
            "ffmpeg input frame geometry mismatch".into(),
        ));
    }
    let planes = frame.planes();

    match fmt {
        PixelFormat::RGB24 | PixelFormat::BGR24 => {
            let plane = planes
                .into_iter()
                .next()
                .ok_or_else(|| CodecError::Codec("RGB24 missing plane".into()))?;
            let src_stride = plane.stride();
            let src = plane.data();
            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            let row_bytes = width as usize * 3;
            for y in 0..height as usize {
                let src_off = y * src_stride;
                let dst_off = y * dst_stride;
                if src_off + row_bytes > src.len() || dst_off + row_bytes > dst_data.len() {
                    return Err(CodecError::Codec("RGB24 plane too short".into()));
                }
                dst_data[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
            Ok(())
        }
        PixelFormat::RGBA | PixelFormat::BGRA => {
            let plane = planes
                .into_iter()
                .next()
                .ok_or_else(|| CodecError::Codec("RGBA missing plane".into()))?;
            let src_stride = plane.stride();
            let src = plane.data();
            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            let row_bytes = width as usize * 4;
            for y in 0..height as usize {
                let src_off = y * src_stride;
                let dst_off = y * dst_stride;
                if src_off + row_bytes > src.len() || dst_off + row_bytes > dst_data.len() {
                    return Err(CodecError::Codec("RGBA plane too short".into()));
                }
                dst_data[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
            Ok(())
        }
        PixelFormat::NV12 => {
            if planes.len() < 2 {
                return Err(CodecError::Codec("NV12 requires 2 planes".into()));
            }
            let y = &planes[0];
            let uv = &planes[1];
            let (w, h) = (width as usize, height as usize);
            if y.data().len() < y.stride().saturating_mul(h) {
                return Err(CodecError::Codec("NV12 Y plane too short".into()));
            }
            if uv.data().len() < uv.stride().saturating_mul(h / 2) {
                return Err(CodecError::Codec("NV12 UV plane too short".into()));
            }

            let dst_y_stride = dst.stride(0);
            let dst_uv_stride = dst.stride(1);
            {
                let dst_y = dst.data_mut(0);
                for row in 0..h {
                    let src_row = &y.data()[row * y.stride()..row * y.stride() + w];
                    let dst_row = &mut dst_y[row * dst_y_stride..row * dst_y_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            {
                let dst_uv = dst.data_mut(1);
                for row in 0..(h / 2) {
                    let src_row = &uv.data()[row * uv.stride()..row * uv.stride() + w];
                    let dst_row = &mut dst_uv[row * dst_uv_stride..row * dst_uv_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            Ok(())
        }
        PixelFormat::YUV420P | PixelFormat::YUVJ420P => {
            if planes.len() < 3 {
                return Err(CodecError::Codec("I420 requires 3 planes".into()));
            }
            let y = &planes[0];
            let u = &planes[1];
            let v = &planes[2];
            let (w, h) = (width as usize, height as usize);
            let cw = w / 2;
            if y.data().len() < y.stride().saturating_mul(h) {
                return Err(CodecError::Codec("I420 Y plane too short".into()));
            }
            if u.data().len() < u.stride().saturating_mul(h / 2)
                || v.data().len() < v.stride().saturating_mul(h / 2)
            {
                return Err(CodecError::Codec("I420 UV plane too short".into()));
            }

            let dst_y_stride = dst.stride(0);
            let dst_u_stride = dst.stride(1);
            let dst_v_stride = dst.stride(2);
            {
                let dst_y = dst.data_mut(0);
                for row in 0..h {
                    let src_row = &y.data()[row * y.stride()..row * y.stride() + w];
                    let dst_row = &mut dst_y[row * dst_y_stride..row * dst_y_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            {
                let dst_u = dst.data_mut(1);
                for row in 0..(h / 2) {
                    let src_u = &u.data()[row * u.stride()..row * u.stride() + cw];
                    let dst_u_row = &mut dst_u[row * dst_u_stride..row * dst_u_stride + cw];
                    dst_u_row.copy_from_slice(src_u);
                }
            }
            {
                let dst_v = dst.data_mut(2);
                for row in 0..(h / 2) {
                    let src_v = &v.data()[row * v.stride()..row * v.stride() + cw];
                    let dst_v_row = &mut dst_v[row * dst_v_stride..row * dst_v_stride + cw];
                    dst_v_row.copy_from_slice(src_v);
                }
            }
            Ok(())
        }
        PixelFormat::YUYV422 => {
            let plane = planes
                .first()
                .ok_or_else(|| CodecError::Codec("YUYV missing plane".into()))?;
            let (w, h) = (width as usize, height as usize);
            let bytes_per_row = w.saturating_mul(2);
            if plane.data().len() < plane.stride().saturating_mul(h) {
                return Err(CodecError::Codec("YUYV plane too short".into()));
            }

            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            for row in 0..h {
                let src_row =
                    &plane.data()[row * plane.stride()..row * plane.stride() + bytes_per_row];
                let dst_row = &mut dst_data[row * dst_stride..row * dst_stride + bytes_per_row];
                dst_row.copy_from_slice(src_row);
            }
            Ok(())
        }
        _ => Err(CodecError::Codec(format!(
            "unsupported ffmpeg input pixel format: {fmt:?}"
        ))),
    }
}

/// Keeps a frame's memory valid while FFmpeg holds it.
pub(super) type Owner = Arc<dyn Any + Send + Sync>;

pub(super) fn backing_owner(backing: Arc<dyn ExternalBacking>) -> Owner {
    Arc::new(backing)
}

/// FFmpeg frames and their SIMD code assume this alignment of plane pointers and strides
/// (`av_frame_get_buffer` uses the same).
const INPUT_ALIGN: usize = 32;

/// `frame` as an FFmpeg frame over its own memory (no copy), when the encoder takes `fmt` at
/// the frame's size and every plane is aligned and long enough; `owner` keeps the memory
/// valid until FFmpeg releases it. Camera buffers (V4L2 mmap, libcamera dma-buf) and aligned
/// pools qualify.
pub(super) fn borrowed_input(
    fmt: PixelFormat,
    frame: &FrameLease,
    owner: Owner,
) -> Option<FfFrame> {
    let res = frame.meta().format.resolution;
    let (w, h) = (res.width.get() as usize, res.height.get() as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    // (rows, visible bytes per row) of each plane.
    let geometry: &[(usize, usize)] = match fmt {
        PixelFormat::RGB24 | PixelFormat::BGR24 => &[(h, w * 3)],
        PixelFormat::RGBA | PixelFormat::BGRA => &[(h, w * 4)],
        PixelFormat::YUYV422 => &[(h, cw * 4)],
        PixelFormat::GRAY8 => &[(h, w)],
        PixelFormat::NV12 => &[(h, w), (ch, cw * 2)],
        PixelFormat::YUV420P | PixelFormat::YUVJ420P => &[(h, w), (ch, cw), (ch, cw)],
        _ => return None,
    };
    let planes = frame.planes();
    if planes.len() < geometry.len() {
        return None;
    }
    let mut described = Vec::with_capacity(geometry.len());
    for (plane, &(rows, row_bytes)) in planes.iter().zip(geometry) {
        let (data, stride) = (plane.data(), plane.stride());
        let needed = stride.checked_mul(rows - 1)?.checked_add(row_bytes)?;
        if stride < row_bytes
            || data.len() < needed
            || !stride.is_multiple_of(INPUT_ALIGN)
            || !(data.as_ptr() as usize).is_multiple_of(INPUT_ALIGN)
        {
            return None;
        }
        described.push((data.as_ptr(), stride, data.len()));
    }
    #[cfg(test)]
    tests::BORROWED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: each plane is `len` readable bytes covering its rows at `stride`, and `owner`
    // (the frame itself, or its driver buffer) keeps them valid and unchanged while FFmpeg
    // holds the frame.
    unsafe { FfFrame::borrowing(fmt, res.width.get(), res.height.get(), &described, owner) }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use smallvec::smallvec;

    use super::*;
    use crate::Codec;
    use crate::ffmpeg::encoder::{FfmpegEncoderOptions, FfmpegH264Encoder};

    /// Frames handed to FFmpeg in place.
    pub(super) static BORROWED: AtomicUsize = AtomicUsize::new(0);

    const W: usize = 320;
    const H: usize = 240;

    /// A driver-style buffer: I420 at `offset` bytes into its allocation, counting releases.
    struct Buffer {
        bytes: Vec<u8>,
        offset: usize,
        released: Arc<AtomicUsize>,
    }

    impl ExternalBacking for Buffer {
        fn plane_data(&self, _index: usize) -> Option<&[u8]> {
            Some(&self.bytes[self.offset..])
        }
    }

    impl Drop for Buffer {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Frame `index` of a moving gradient, with its planes `misalign` bytes off alignment.
    fn frame(index: usize, misalign: usize, released: &Arc<AtomicUsize>) -> FrameLease {
        let len = W * H * 3 / 2;
        let mut bytes = vec![0u8; len + 64 + misalign];
        let offset = (64 - bytes.as_ptr() as usize % 64) % 64 + misalign;
        for (i, px) in bytes[offset..offset + len].iter_mut().enumerate() {
            *px = (i % W + i / W + index * 3) as u8;
        }
        let res = Resolution::new(W as u32, H as u32).unwrap();
        let meta = FrameMeta::new(
            MediaFormat::new(FourCc::I420, res, ColorSpace::Bt709),
            index as u64,
        );
        let (y, c) = (W * H, W * H / 4);
        let plane = |offset, len, stride| PlaneLayout {
            offset,
            len,
            stride,
        };
        FrameLease::from_external(
            meta,
            smallvec![plane(0, y, W), plane(y, c, W / 2), plane(y + c, c, W / 2)],
            Arc::new(Buffer {
                bytes,
                offset,
                released: released.clone(),
            }),
        )
    }

    fn encode(misalign: usize) -> Option<(Vec<u8>, usize)> {
        let encoder = FfmpegH264Encoder::with_options_for_input(
            FourCc::I420,
            FfmpegEncoderOptions {
                thread_count: Some(1),
                ..Default::default()
            },
        )
        .ok()?
        .0;
        if !encoder.is_available() {
            return None;
        }
        let released = Arc::new(AtomicUsize::new(0));
        let mut stream = Vec::new();
        for index in 0..12 {
            match encoder.process(frame(index, misalign, &released)) {
                Ok(packet) => stream.extend_from_slice(packet.planes()[0].data()),
                Err(crate::CodecError::Backpressure) => {}
                Err(err) => panic!("encode failed: {err}"),
            }
        }
        let meta = FrameMeta::new(
            MediaFormat::new(
                FourCc::I420,
                Resolution::new(W as u32, H as u32)?,
                ColorSpace::Bt709,
            ),
            12,
        );
        for packet in encoder.flush_encoder(&meta).ok()? {
            stream.extend_from_slice(packet.planes()[0].data());
        }
        drop(encoder);
        Some((stream, released.load(Ordering::Relaxed)))
    }

    #[test]
    fn aligned_frames_are_encoded_in_place_with_the_same_result() {
        let before = BORROWED.load(Ordering::Relaxed);
        // Skipped where FFmpeg (or an H.264 encoder) is not installed.
        let Some((copied, copied_released)) = encode(3) else {
            return;
        };
        assert_eq!(
            BORROWED.load(Ordering::Relaxed),
            before,
            "misaligned frames are copied"
        );
        let (in_place, in_place_released) = encode(0).expect("encoder");
        assert_eq!(BORROWED.load(Ordering::Relaxed) - before, 12);
        assert!(!in_place.is_empty());
        assert_eq!(
            in_place, copied,
            "same bitstream whether copied or read in place"
        );
        // Every driver buffer went back once FFmpeg was done with it.
        assert_eq!((copied_released, in_place_released), (12, 12));
    }
}

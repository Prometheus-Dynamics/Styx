/// Convert an owned `FrameLease` into a `DynamicImage`.
///
/// This is preferred over [`frame_to_dynamic_image`] when the caller can give up the frame, as it
/// can avoid copies for tightly-packed CPU frames (e.g. `RG24`, `RGBA`, `R8  `).
///
/// This function intentionally only handles packed CPU formats that can be wrapped cheaply.
/// For planar formats (e.g. `NV12`, `YUYV`) and raw sensor formats (e.g. Bayer), prefer routing
/// through the codec registry so the fastest available decoder can be selected.
///
/// If the format is not supported by this fast-path, the original frame is returned as
/// `Err(frame)` so the caller can route it through a decoder.
// Returning the original frame avoids an allocation and preserves ownership for fallback decode.
#[allow(clippy::result_large_err)]
pub fn frame_lease_to_dynamic_image(frame: FrameLease) -> Result<DynamicImage, FrameLease> {
    let code = frame.meta().format.code;

    let is_packed = code == FourCc::R8
        || code == FourCc::GREY
        || code == FourCc::NV12
        || code == FourCc::NV21
        || code == FourCc::RG24
        || code == FourCc::RGBA;

    if !is_packed {
        return Err(frame);
    }

    let meta = frame.meta();
    let width = meta.format.resolution.width.get();
    let height = meta.format.resolution.height.get();

    if frame.is_external() {
        if code == FourCc::NV12 || code == FourCc::NV21 {
            let planes = frame.planes();
            if planes.is_empty() {
                drop(planes);
                return Err(frame);
            }
            let plane = &planes[0];
            let stride = plane.stride().max(width as usize);
            let expected = width as usize;
            let required = stride.saturating_mul(height as usize);
            if plane.data().len() < required {
                drop(planes);
                return Err(frame);
            }
            let out = if stride == expected {
                let required = expected.saturating_mul(height as usize);
                plane.data()[..required].to_vec()
            } else {
                let required = expected.saturating_mul(height as usize);
                let mut out = vec![0u8; required];
                let dst: *mut u8 = out.as_mut_ptr();
                let src: *const u8 = plane.data().as_ptr();
                for y in 0..height as usize {
                    let src_off = y.saturating_mul(stride);
                    let dst_off = y.saturating_mul(expected);
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected);
                    }
                }
                out
            };
            drop(planes);
            let Some(img) = image::GrayImage::from_raw(width, height, out) else {
                return Err(frame);
            };
            return Ok(DynamicImage::ImageLuma8(img));
        }
        return frame_to_dynamic_image(&frame).ok_or(frame);
    }

    if code == FourCc::NV12 || code == FourCc::NV21 {
        let layouts = frame.layouts();
        if let Some(layout) = layouts.first() {
            let expected = width as usize;
            let stride = layout.stride.max(expected);
            let required = stride.saturating_mul(height as usize);
            if layout.offset == 0 && stride == expected {
                let planes = frame.planes();
                if let Some(plane) = planes.first()
                    && plane.data().len() >= required
                    && layout.len >= required
                {
                    drop(planes);
                    let parts = frame.into_parts();
                    let mut buf = match parts.buffers.into_iter().next() {
                        Some(buf) => buf,
                        None => {
                            let img = image::GrayImage::new(width, height);
                            return Ok(DynamicImage::ImageLuma8(img));
                        }
                    };
                    buf.truncate(required);
                    let img = image::GrayImage::from_raw(width, height, buf)
                        .unwrap_or_else(|| image::GrayImage::new(width, height));
                    return Ok(DynamicImage::ImageLuma8(img));
                }
            }
        }
        let planes = frame.planes();
        if planes.is_empty() {
            drop(planes);
            return Err(frame);
        }
        let plane = &planes[0];
        let stride = plane.stride().max(width as usize);
        let expected = width as usize;
        let required = stride.saturating_mul(height as usize);
        if plane.data().len() < required {
            drop(planes);
            return Err(frame);
        }
        let out = if stride == expected {
            let required = expected.saturating_mul(height as usize);
            plane.data()[..required].to_vec()
        } else {
            let required = expected.saturating_mul(height as usize);
            let mut out = vec![0u8; required];
            let dst: *mut u8 = out.as_mut_ptr();
            let src: *const u8 = plane.data().as_ptr();
            for y in 0..height as usize {
                let src_off = y.saturating_mul(stride);
                let dst_off = y.saturating_mul(expected);
                unsafe {
                    std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected);
                }
            }
            out
        };
        drop(planes);
        let Some(img) = image::GrayImage::from_raw(width, height, out) else {
            return Err(frame);
        };
        return Ok(DynamicImage::ImageLuma8(img));
    }

    let (bytes_per_pixel, wrap) = if code == FourCc::R8 || code == FourCc::GREY
    {
        (1usize, 0u8)
    } else if code == FourCc::RG24 {
        (3usize, 1u8)
    } else if code == FourCc::RGBA {
        (4usize, 2u8)
    } else {
        return Err(frame);
    };
    let expected_stride = (width as usize).saturating_mul(bytes_per_pixel);

    let planes = frame.planes();
    if planes.is_empty() {
        drop(planes);
        return Err(frame);
    }
    let plane_stride = planes[0].stride();
    let plane_len = planes[0].data().len();
    drop(planes);
    let stride = plane_stride.max(expected_stride);
    let required = stride.saturating_mul(height as usize);
    if plane_len < required {
        return Err(frame);
    }

    let can_take_zero_copy = plane_stride == expected_stride && plane_len >= required;
    if can_take_zero_copy {
        let mut parts = frame.into_parts();
        let layout = *parts
            .layouts
            .first()
            .expect("frame has at least one plane layout");
        let buf = parts
            .buffers
            .pop()
            .expect("non-external packed frame has an owned buffer");

        let stride = layout.stride.max(expected_stride);
        let required = stride.saturating_mul(height as usize);
        if layout.offset == 0
            && stride == expected_stride
            && layout.len >= required
            && buf.len() >= required
        {
            let mut buf = buf;
            buf.truncate(required);
            match wrap {
                0 => {
                    let img =
                        image::GrayImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageLuma8(img));
                }
                1 => {
                    let img =
                        image::RgbImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageRgb8(img));
                }
                _ => {
                    let img =
                        image::RgbaImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageRgba8(img));
                }
            }
        }

        let data = buf
            .get(layout.offset..layout.offset.saturating_add(layout.len))
            .unwrap_or(&[]);
        return Ok(copy_packed_to_image(
            code,
            width,
            height,
            expected_stride,
            stride,
            data,
        ));
    }

    let planes = frame.planes();
    let plane = &planes[0];
    Ok(copy_packed_to_image(
        code,
        width,
        height,
        expected_stride,
        stride,
        plane.data(),
    ))
}

fn copy_packed_to_image(
    code: FourCc,
    width: u32,
    height: u32,
    expected_stride: usize,
    stride: usize,
    data: &[u8],
) -> DynamicImage {
    fn copy_strided(
        out: &mut Vec<u8>,
        expected_stride: usize,
        stride: usize,
        height: u32,
        data: &[u8],
    ) {
        let height = height as usize;
        let required = expected_stride.saturating_mul(height);
        out.clear();
        out.resize(required, 0);
        let dst = out.as_mut_ptr();
        let src = data.as_ptr();
        for y in 0..height {
            let src_off = y.saturating_mul(stride);
            let dst_off = y.saturating_mul(expected_stride);
            unsafe {
                std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected_stride);
            }
        }
    }

    match code {
        c if c == FourCc::R8 || c == FourCc::GREY => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageLuma8(
                image::GrayImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        c if c == FourCc::RG24 => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        c if c == FourCc::RGBA => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        _ => unreachable!("copy_packed_to_image only called for supported packed formats"),
    }
}

pub fn dynamic_image_to_rg24_frame(img: DynamicImage, timestamp: u64) -> Option<FrameLease> {
    match img {
        DynamicImage::ImageRgb8(rgb) => {
            let (width, height) = rgb.dimensions();
            let res = Resolution::new(width, height)?;
            let stride = (width as usize) * 3;
            let len = stride.checked_mul(height as usize)?;
            let raw = rgb.into_raw();
            if raw.len() != len {
                return None;
            }
            let pool = packed_frame_pool(len);
            let mut buf = pool.lease();
            buf.replace_owned(raw);
            let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
            Some(FrameLease::single_plane(
                FrameMeta::new(format, timestamp),
                buf,
                len,
                stride,
            ))
        }
        other => {
            let rgb = other.into_rgb8();
            let (width, height) = rgb.dimensions();
            let res = Resolution::new(width, height)?;
            let stride = (width as usize) * 3;
            let len = stride.checked_mul(height as usize)?;
            let pool = packed_frame_pool(len);
            let mut buf = pool.lease();
            buf.resize(len);
            buf.as_mut_slice().copy_from_slice(&rgb);
            let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
            Some(FrameLease::single_plane(
                FrameMeta::new(format, timestamp),
                buf,
                len,
                stride,
            ))
        }
    }
}

pub fn dynamic_image_ref_to_rg24_frame(img: &DynamicImage, timestamp: u64) -> Option<FrameLease> {
    let (width, height) = img.dimensions();
    let res = Resolution::new(width, height)?;
    let stride = (width as usize) * 3;
    let len = stride.checked_mul(height as usize)?;
    let pool = packed_frame_pool(len);
    let mut buf = pool.lease();
    buf.resize(len);
    if let Some(rgb) = img.as_rgb8() {
        let raw = rgb.as_raw();
        if raw.len() < len {
            return None;
        }
        buf.as_mut_slice().copy_from_slice(&raw[..len]);
    } else {
        let rgb = img.to_rgb8();
        let raw = rgb.as_raw();
        if raw.len() < len {
            return None;
        }
        buf.as_mut_slice().copy_from_slice(&raw[..len]);
    }

    let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
    Some(FrameLease::single_plane(
        FrameMeta::new(format, timestamp),
        buf,
        len,
        stride,
    ))
}

include!("image_compat_frame_to_dynamic.incl.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_rgb_buffer() {
        let res = Resolution::new(2, 2).unwrap();
        let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let stride = res.width.get() as usize * 3;
        let len = stride * res.height.get() as usize - 1;
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        let frame = FrameLease::single_plane(FrameMeta::new(format, 0), buf, len, stride);
        assert!(frame_to_dynamic_image(&frame).is_none());
    }

    struct NoopCodec {
        desc: crate::CodecDescriptor,
    }

    impl NoopCodec {
        fn new(input: FourCc) -> Self {
            Self {
                desc: crate::CodecDescriptor {
                    kind: crate::CodecKind::Decoder,
                    input,
                    output: input,
                    name: "noop",
                    impl_name: "noop",
                },
            }
        }
    }

    impl crate::Codec for NoopCodec {
        fn descriptor(&self) -> &crate::CodecDescriptor {
            &self.desc
        }

        fn process(&self, input: FrameLease) -> Result<FrameLease, crate::CodecError> {
            Ok(input)
        }
    }

    #[test]
    fn process_to_dynamic_prefers_input_conversion() {
        let res = Resolution::new(2, 1).unwrap();
        let format = MediaFormat::new(FourCc::BGRA, res, ColorSpace::Srgb);
        let stride = res.width.get() as usize * 4;
        let len = stride * res.height.get() as usize;
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        buf.as_mut_slice()
            .copy_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
        let frame = FrameLease::single_plane(FrameMeta::new(format, 0), buf, len, stride);

        let codec = NoopCodec::new(FourCc::BGRA);
        let out = process_to_dynamic(&codec, frame).unwrap();
        let rgba = out.into_rgba8();
        assert_eq!(rgba.as_raw(), &[0, 0, 255, 255, 255, 0, 0, 255]);
    }
}

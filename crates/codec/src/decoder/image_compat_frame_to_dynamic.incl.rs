pub fn frame_to_dynamic_image(frame: &FrameLease) -> Option<DynamicImage> {
    let meta = frame.meta();
    let res = meta.format.resolution;
    let width = res.width.get();
    let height = res.height.get();
    let color = meta.format.color;
    let code = meta.format.code;
    let planes = frame.planes();

    #[inline(always)]
    fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
        match color {
            ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
            ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
            ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
            ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        }
    }

    fn copy_tightly_packed(src: &[u8], len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        out.copy_from_slice(&src[..len]);
        out
    }

    fn copy_strided_packed_external(
        plane_data: &[u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
    ) -> Vec<u8> {
        let required_src = src_stride.saturating_mul(height);
        const STAGE_THRESHOLD_BYTES: usize = 256 * 1024;
        let use_contiguous_stage =
            plane_data.len() >= required_src && required_src >= STAGE_THRESHOLD_BYTES;

        if use_contiguous_stage {
            record_staging_copy(required_src);
            let mut staged = vec![0u8; required_src];
            staged.copy_from_slice(&plane_data[..required_src]);
            return copy_strided_packed(&staged, src_stride, dst_stride, height);
        }

        copy_strided_packed(plane_data, src_stride, dst_stride, height)
    }

    fn copy_strided_packed(
        plane_data: &[u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
    ) -> Vec<u8> {
        let required_dst = dst_stride.saturating_mul(height);
        let mut out: Vec<u8> = vec![0u8; required_dst];
        let dst: *mut u8 = out.as_mut_ptr();
        let src: *const u8 = plane_data.as_ptr();
        for y in 0..height {
            let src_off = y.saturating_mul(src_stride);
            let dst_off = y.saturating_mul(dst_stride);
            unsafe {
                std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), dst_stride);
            }
        }
        out
    }

    #[inline(always)]
    fn convert_strided_bgr_to_rgb(
        width: usize,
        height: usize,
        src: &[u8],
        src_stride: usize,
    ) -> Vec<u8> {
        let dst_stride = width * 3;
        let required = dst_stride.saturating_mul(height);
        let mut out = vec![0u8; required];

        let out_ptr: *mut u8 = out.as_mut_ptr();
        for y in 0..height {
            let src_line = &src[y * src_stride..][..width * 3];
            let dst_line =
                unsafe { std::slice::from_raw_parts_mut(out_ptr.add(y * dst_stride), dst_stride) };

            styx_core::simd::swap_rb24_row(src_line, dst_line, width);
        }
        out
    }

    #[inline(always)]
    fn convert_strided_bgra_to_rgba(
        width: usize,
        height: usize,
        src: &[u8],
        src_stride: usize,
    ) -> Vec<u8> {
        let dst_stride = width * 4;
        let required = dst_stride.saturating_mul(height);
        let mut out = vec![0u8; required];

        let out_ptr: *mut u8 = out.as_mut_ptr();
        for y in 0..height {
            let src_line = &src[y * src_stride..][..width * 4];
            let dst_line =
                unsafe { std::slice::from_raw_parts_mut(out_ptr.add(y * dst_stride), dst_stride) };

            styx_core::simd::swap_rb32_row(src_line, dst_line, width);
        }
        out
    }

    match code {
        c if c == FourCc::R8 || c == FourCc::GREY => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let expected = width as usize;
            if stride == expected {
                let required = expected.checked_mul(height as usize)?;
                let out = copy_tightly_packed(plane.data(), required);
                return image::GrayImage::from_raw(width, height, out).map(DynamicImage::ImageLuma8);
            }
            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
            image::GrayImage::from_raw(width, height, out).map(DynamicImage::ImageLuma8)
        }
        c if c == FourCc::RG24 => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 3);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let expected = width as usize * 3;
            if stride == expected {
                let required = expected.checked_mul(height as usize)?;
                let out = copy_tightly_packed(plane.data(), required);
                return image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8);
            }
            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::RGBA => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let expected = width as usize * 4;
            if stride == expected {
                let required = expected.checked_mul(height as usize)?;
                let out = copy_tightly_packed(plane.data(), required);
                return image::RgbaImage::from_raw(width, height, out)
                    .map(DynamicImage::ImageRgba8);
            }
            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
            image::RgbaImage::from_raw(width, height, out).map(DynamicImage::ImageRgba8)
        }
        c if c == FourCc::BGR3 || c == FourCc::BG24 => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 3);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let out = convert_strided_bgr_to_rgb(
                width as usize,
                height as usize,
                &plane.data()[..required],
                stride,
            );
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::BGRA => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let out = convert_strided_bgra_to_rgba(
                width as usize,
                height as usize,
                &plane.data()[..required],
                stride,
            );
            image::RgbaImage::from_raw(width, height, out).map(DynamicImage::ImageRgba8)
        }
        c if c == FourCc::XB24 || c == FourCc::XR24 => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let dst_stride = width as usize * 3;
            let len = dst_stride.checked_mul(height as usize)?;
            let mut out = vec![0u8; len];
            let src = &plane.data()[..required];
            let xb24 = c == FourCc::XB24;
            crate::par::for_each_row(&mut out[..], dst_stride, |y, dst_line| {
                let start = y * stride;
                let src_line = &src[start..start + (width as usize * 4)];
                styx_core::simd::x32_to_rgb24_row(src_line, dst_line, width as usize, xb24);
            });
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::YUYV => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max((width as usize) * 2);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let packed = YuvPackedImage {
                yuy: &plane.data()[..required],
                yuy_stride: stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            if yuvutils_rs::yuyv422_to_rgb(&packed, &mut rgb, dst_stride as u32, range, matrix)
                .is_err()
            {
                let src = &plane.data()[..required];
                crate::par::for_each_row(&mut rgb[..], dst_stride, |y, dst_line| {
                    let line = &src[y * stride..][..(width as usize) * 2];
                    let pair_count = (width as usize) / 2;
                    for pair in 0..pair_count {
                        let si = pair * 4;
                        let di = pair * 6;
                        let y0 = line[si] as i32;
                        let u = line[si + 1] as i32;
                        let y1 = line[si + 2] as i32;
                        let v = line[si + 3] as i32;
                        let (r0, g0, b0) = yuv_to_rgb(y0, u, v, color);
                        let (r1, g1, b1) = yuv_to_rgb(y1, u, v, color);
                        dst_line[di] = r0;
                        dst_line[di + 1] = g0;
                        dst_line[di + 2] = b0;
                        dst_line[di + 3] = r1;
                        dst_line[di + 4] = g1;
                        dst_line[di + 5] = b1;
                    }
                    if (width as usize) % 2 == 1 && (width as usize) >= 1 {
                        let last_x = (width as usize) - 1;
                        let si = (last_x / 2) * 4;
                        let di = last_x * 3;
                        let yv = line[si] as i32;
                        let u = line[si + 1] as i32;
                        let v = line[si + 3] as i32;
                        let (r, g, b) = yuv_to_rgb(yv, u, v, color);
                        dst_line[di] = r;
                        dst_line[di + 1] = g;
                        dst_line[di + 2] = b;
                    }
                });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::NV12 || c == FourCc::NV21 => {
            if planes.len() < 2 {
                return None;
            }
            let y_plane = &planes[0];
            let uv_plane = &planes[1];
            let y_stride = y_plane.stride().max(width as usize);
            let chroma_width = (width as usize).div_ceil(2);
            let uv_stride = uv_plane.stride().max(chroma_width * 2);
            let chroma_height = (height as usize).div_ceil(2);
            let y_required = y_stride.checked_mul(height as usize)?;
            let uv_required = uv_stride.checked_mul(chroma_height)?;
            if y_plane.data().len() < y_required || uv_plane.data().len() < uv_required {
                return None;
            }

            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let bi = YuvBiPlanarImage {
                y_plane: &y_plane.data()[..y_required],
                y_stride: y_stride as u32,
                uv_plane: &uv_plane.data()[..uv_required],
                uv_stride: uv_stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            let mode = preferred_yuv_conversion_mode();
            let is_nv12 = c == FourCc::NV12;
            let ok = if is_nv12 {
                yuvutils_rs::yuv_nv12_to_rgb(&bi, &mut rgb, dst_stride as u32, range, matrix, mode)
            } else {
                yuvutils_rs::yuv_nv21_to_rgb(&bi, &mut rgb, dst_stride as u32, range, matrix, mode)
            };
            if ok.is_err() {
                let y_data = &y_plane.data()[..y_required];
                let uv_data = &uv_plane.data()[..uv_required];
                crate::par::for_each_row(&mut rgb[..], dst_stride, |y, dst_line| {
                    let y_line = &y_data[y * y_stride..][..width as usize];
                    let uv_line = &uv_data[(y / 2) * uv_stride..][..chroma_width * 2];
                    for (x, yv) in y_line.iter().enumerate() {
                        let uv_idx = (x / 2) * 2;
                        let (u, v) = if is_nv12 {
                            (uv_line[uv_idx] as i32, uv_line[uv_idx + 1] as i32)
                        } else {
                            (uv_line[uv_idx + 1] as i32, uv_line[uv_idx] as i32)
                        };
                        let (r, g, b) = yuv_to_rgb(*yv as i32, u, v, color);
                        let di = x * 3;
                        dst_line[di] = r;
                        dst_line[di + 1] = g;
                        dst_line[di + 2] = b;
                    }
                });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::I420 => {
            if planes.len() < 3 {
                return None;
            }
            let y_plane = &planes[0];
            let u_plane = &planes[1];
            let v_plane = &planes[2];
            let y_stride = y_plane.stride().max(width as usize);
            let chroma_width = (width as usize).div_ceil(2);
            let chroma_height = (height as usize).div_ceil(2);
            let u_stride = u_plane.stride().max(chroma_width);
            let v_stride = v_plane.stride().max(chroma_width);
            let y_required = y_stride.checked_mul(height as usize)?;
            let u_required = u_stride.checked_mul(chroma_height)?;
            let v_required = v_stride.checked_mul(chroma_height)?;
            if y_plane.data().len() < y_required
                || u_plane.data().len() < u_required
                || v_plane.data().len() < v_required
            {
                return None;
            }
            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let planar = YuvPlanarImage {
                y_plane: &y_plane.data()[..y_required],
                y_stride: y_stride as u32,
                u_plane: &u_plane.data()[..u_required],
                u_stride: u_stride as u32,
                v_plane: &v_plane.data()[..v_required],
                v_stride: v_stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            if yuvutils_rs::yuv420_to_rgb(&planar, &mut rgb, dst_stride as u32, range, matrix)
                .is_err()
            {
                let y_data = &y_plane.data()[..y_required];
                let u_data = &u_plane.data()[..u_required];
                let v_data = &v_plane.data()[..v_required];
                crate::par::for_each_row(&mut rgb[..], dst_stride, |y, dst_line| {
                    let y_line = &y_data[y * y_stride..][..width as usize];
                    let u_line = &u_data[(y / 2) * u_stride..][..chroma_width];
                    let v_line = &v_data[(y / 2) * v_stride..][..chroma_width];
                    for (x, yv) in y_line.iter().enumerate() {
                        let u = u_line[x / 2] as i32;
                        let v = v_line[x / 2] as i32;
                        let (r, g, b) = yuv_to_rgb(*yv as i32, u, v, color);
                        let di = x * 3;
                        dst_line[di] = r;
                        dst_line[di + 1] = g;
                        dst_line[di + 2] = b;
                    }
                });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        _ => None,
    }
}

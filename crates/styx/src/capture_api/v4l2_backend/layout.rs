//! Single-plane buffer layouts for V4L2 capture formats.

use super::*;

pub(super) fn is_encoded_bitstream(code: FourCc) -> bool {
    code.is_compressed()
}

pub(super) fn supports_v4l2_mmap_zero_copy(code: FourCc) -> bool {
    code.layout_info().planes.subsampling.is_some()
        || matches!(
            &code.to_u32().to_le_bytes(),
            b"MJPG"
                | b"JPEG"
                | b"YUYV"
                | b"RG24"
                | b"RGB3"
                | b"BGR3"
                | b"RGBA"
                | b"BGRA"
                | b"GREY"
                | b"R8  "
        )
}

pub(super) fn build_v4l2_single_plane_layout(
    encoded: bool,
    height: usize,
    stride: usize,
    bytes_used: usize,
) -> Option<PlaneLayout> {
    if encoded {
        return Some(PlaneLayout {
            offset: 0,
            len: bytes_used,
            stride: bytes_used.max(1),
        });
    }
    if height == 0 || stride == 0 {
        return None;
    }
    let required = height.saturating_mul(stride);
    if bytes_used < required {
        return None;
    }
    Some(PlaneLayout {
        offset: 0,
        len: required,
        stride,
    })
}

#[derive(Debug, Clone)]
pub(super) struct V4l2SinglePlaneLayoutPlan {
    /// First plane (or the whole payload for packed and encoded formats).
    pub(super) layout: PlaneLayout,
    /// Every plane inside the buffer; more than one for planar/semi-planar YUV.
    pub(super) planes: SmallVec<[PlaneLayout; 3]>,
    pub(super) zero_copy_safe: bool,
}

pub(super) fn plan_v4l2_single_plane_layout(
    code: FourCc,
    width: usize,
    height: usize,
    negotiated_stride: usize,
    negotiated_size: usize,
    mapped_len: usize,
    bytes_used: usize,
) -> Option<V4l2SinglePlaneLayoutPlan> {
    if bytes_used == 0 || bytes_used > mapped_len {
        return None;
    }

    let encoded = is_encoded_bitstream(code);
    if encoded {
        let layout = build_v4l2_single_plane_layout(true, height, 0, bytes_used)?;
        return Some(V4l2SinglePlaneLayoutPlan {
            layout,
            planes: smallvec![layout],
            zero_copy_safe: layout.len <= mapped_len,
        });
    }

    // Planar/semi-planar YUV: the Y stride is `bytesperline`, not `bytesused / height`.
    if let Some(planes) =
        yuv_layout::yuv_plane_layouts(code, width, height, negotiated_stride, bytes_used)
    {
        let end = planes.last().map_or(0, |plane| plane.offset + plane.len);
        return Some(V4l2SinglePlaneLayoutPlan {
            layout: planes[0],
            zero_copy_safe: (negotiated_size == 0 || end <= negotiated_size) && end <= mapped_len,
            planes,
        });
    }

    let min_stride = min_stride_for_fourcc(code, width).max(1);
    let inferred_stride = bytes_used.checked_div(height).unwrap_or(0);
    let stride = negotiated_stride.max(inferred_stride).max(min_stride);
    let required = height.checked_mul(stride)?;
    if required == 0 || bytes_used < required {
        return None;
    }

    let layout = build_v4l2_single_plane_layout(false, height, stride, bytes_used)?;
    let advertised_capacity_ok = negotiated_size == 0 || layout.len <= negotiated_size;
    Some(V4l2SinglePlaneLayoutPlan {
        layout,
        planes: smallvec![layout],
        zero_copy_safe: advertised_capacity_ok && layout.len <= mapped_len,
    })
}

pub(super) fn min_stride_for_fourcc(code: FourCc, width: usize) -> usize {
    match &code.to_u32().to_le_bytes() {
        // MIPI packed RAW10/RAW12 bayer.
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" => width.div_ceil(4) * 5,
        b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" => width.div_ceil(2) * 3,

        // 8-bit bayer and 8-bit mono.
        b"BA81" | b"RGGB" | b"GRBG" | b"GBRG" | b"BGGR" | b"GREY" | b"R8  " => width,

        // 10/12/14/16-bit bayer (stored in 16-bit words) and mono16.
        b"BA10" | b"BA12" | b"BA14" | b"BG10" | b"BG12" | b"BG14" | b"BG16" | b"GB10" | b"GB12"
        | b"GB14" | b"GB16" | b"RG10" | b"RG12" | b"RG14" | b"RG16" | b"GR10" | b"GR12"
        | b"GR14" | b"GR16" | b"BYR2" | b"R16 " => width.saturating_mul(2),

        // Common packed YUV/RGB defaults.
        b"YUYV" => width.saturating_mul(2),
        b"NV12" => width, // luma plane; backend uses bytesused/stride anyway
        b"RG24" | b"RGB3" | b"BGR3" => width.saturating_mul(3),
        b"RGBA" | b"BGRA" | b"RGB0" | b"BGR0" => width.saturating_mul(4),
        _ => width.saturating_mul(3),
    }
}

//! Plane layout construction and validation helpers.

// The validation helpers serve `FrameLease` (std).
#![cfg_attr(not(feature = "std"), allow(dead_code))]

use core::num::NonZeroU32;

use smallvec::{SmallVec, smallvec};

use super::meta::FrameResidency;
use super::plane::{FrameValidationError, PlaneLayout};
use crate::format::{ChromaSubsampling, FrameLayoutInfo, FrameStorageKind, MediaFormat};

pub fn plane_layout_from_dims(
    width: NonZeroU32,
    height: NonZeroU32,
    bytes_per_pixel: usize,
) -> PlaneLayout {
    let stride = width.get() as usize * bytes_per_pixel;
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

pub fn plane_layout_with_stride(
    _width: NonZeroU32,
    height: NonZeroU32,
    stride: usize,
) -> PlaneLayout {
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

pub(super) fn default_layouts_for_format(
    format: MediaFormat,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<SmallVec<[PlaneLayout; 3]>, FrameValidationError> {
    let width = format.resolution.width.get() as usize;
    let height = format.resolution.height.get() as usize;
    let info = format.code.layout_info();
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            let row_bytes = info
                .first_plane_visible_row_bytes(width)
                .ok_or(FrameValidationError::UnknownStorageLayout)?;
            Ok(smallvec![plane_layout_for_visible(
                row_bytes,
                height,
                stride_alignment,
                plane_alignment,
            )?])
        }
        FrameStorageKind::SemiPlanar => match info.planes.subsampling {
            Some(ChromaSubsampling::Cs420) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width,
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs422) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
            ]),
            Some(ChromaSubsampling::Cs444) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width * 2, height, stride_alignment, plane_alignment)?,
            ]),
            None => Err(FrameValidationError::UnknownStorageLayout),
        },
        FrameStorageKind::Planar => match info.planes.subsampling {
            Some(ChromaSubsampling::Cs420) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs422) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height,
                    stride_alignment,
                    plane_alignment,
                )?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height,
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs444) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
            ]),
            None => Ok(smallvec![plane_layout_for_visible(
                width,
                height,
                stride_alignment,
                plane_alignment,
            )?]),
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            Err(FrameValidationError::UnknownStorageLayout)
        }
    }
}

pub(super) fn plane_layout_for_visible(
    row_bytes: usize,
    rows: usize,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<PlaneLayout, FrameValidationError> {
    let stride = align_up(row_bytes, stride_alignment)?;
    let len = align_up(stride.saturating_mul(rows), plane_alignment)?;
    Ok(PlaneLayout {
        offset: 0,
        len,
        stride,
    })
}

pub(super) fn validate_layouts_for_info(
    layouts: &[PlaneLayout],
    info: FrameLayoutInfo,
    width: usize,
    height: usize,
) -> Result<(), FrameValidationError> {
    match info.storage {
        FrameStorageKind::Compressed => {
            if layouts.is_empty() {
                Err(FrameValidationError::NoPlanes)
            } else {
                Ok(())
            }
        }
        FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            Err(FrameValidationError::UnknownStorageLayout)
        }
        _ => {
            for index in 0..info.planes.planes {
                let row_bytes = visible_row_bytes_for_plane(info, width, index)
                    .ok_or(FrameValidationError::UnknownStorageLayout)?;
                let rows = visible_rows_for_plane(info, height, index)
                    .ok_or(FrameValidationError::UnknownStorageLayout)?;
                validate_plane_layout(index, layouts.get(index), row_bytes, rows)?;
            }
            Ok(())
        }
    }
}

pub(super) fn validate_plane_ranges_do_not_overlap(
    layouts: &[PlaneLayout],
) -> Result<(), FrameValidationError> {
    for (left_index, left) in layouts.iter().enumerate() {
        let left_start = left.offset;
        let left_end = left.offset.saturating_add(left.len);
        for (right_index, right) in layouts.iter().enumerate().skip(left_index + 1) {
            let right_start = right.offset;
            let right_end = right.offset.saturating_add(right.len);
            if left_start < right_end && right_start < left_end {
                return Err(FrameValidationError::PlaneRangeOverlap {
                    left: left_index,
                    right: right_index,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_alignment(alignment: Option<usize>) -> Result<(), FrameValidationError> {
    let Some(alignment) = alignment else {
        return Ok(());
    };
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(FrameValidationError::InvalidAlignment(alignment));
    }
    Ok(())
}

pub(super) fn align_up(
    value: usize,
    alignment: Option<usize>,
) -> Result<usize, FrameValidationError> {
    let Some(alignment) = alignment else {
        return Ok(value);
    };
    validate_alignment(Some(alignment))?;
    Ok(value.saturating_add(alignment - 1) & !(alignment - 1))
}

pub(super) fn visible_row_bytes_for_plane(
    info: FrameLayoutInfo,
    width: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => (plane_index == 0)
            .then(|| info.first_plane_visible_row_bytes(width))
            .flatten(),
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0 | 1) => Some(width),
            (Some(ChromaSubsampling::Cs444), 0) => Some(width),
            (Some(ChromaSubsampling::Cs444), 1) => width.checked_mul(2),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0) => Some(width),
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 1 | 2) => {
                Some(width.div_ceil(2))
            }
            (Some(ChromaSubsampling::Cs444), 0..=2) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

pub(super) fn visible_width_for_plane(
    info: FrameLayoutInfo,
    width: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            (plane_index == 0).then_some(width)
        }
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(_), 0 | 1) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0) => Some(width),
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 1 | 2) => {
                Some(width.div_ceil(2))
            }
            (Some(ChromaSubsampling::Cs444), 0..=2) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

pub(super) fn visible_rows_for_plane(
    info: FrameLayoutInfo,
    height: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            (plane_index == 0).then_some(height)
        }
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420), 0) => Some(height),
            (Some(ChromaSubsampling::Cs420), 1) => Some(height.div_ceil(2)),
            (Some(ChromaSubsampling::Cs422 | ChromaSubsampling::Cs444), 0 | 1) => Some(height),
            (None, 0) => Some(height),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420), 0) => Some(height),
            (Some(ChromaSubsampling::Cs420), 1 | 2) => Some(height.div_ceil(2)),
            (Some(ChromaSubsampling::Cs422 | ChromaSubsampling::Cs444), 0..=2) => Some(height),
            (None, 0) => Some(height),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

pub(super) fn validate_plane_layout(
    index: usize,
    layout: Option<&PlaneLayout>,
    visible_row_bytes: usize,
    height: usize,
) -> Result<(), FrameValidationError> {
    let Some(layout) = layout else {
        return Err(FrameValidationError::NoPlanes);
    };
    if layout.stride < visible_row_bytes {
        return Err(FrameValidationError::PlaneStrideTooSmall {
            index,
            stride: layout.stride,
            visible_row_bytes,
        });
    }
    // The last row needs only its visible bytes: crop views end right after them.
    let expected_len = match height {
        0 => 0,
        _ => layout
            .stride
            .saturating_mul(height - 1)
            .saturating_add(visible_row_bytes),
    };
    if layout.len < expected_len {
        return Err(FrameValidationError::PlaneLenTooSmall {
            index,
            len: layout.len,
            expected_len,
        });
    }
    Ok(())
}

pub(super) fn default_owned_residency(code: crate::format::FourCc) -> FrameResidency {
    if code.is_compressed() {
        FrameResidency::CompressedPacket
    } else {
        FrameResidency::HostOwned
    }
}

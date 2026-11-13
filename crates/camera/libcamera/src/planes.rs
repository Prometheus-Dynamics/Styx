//! Where a libcamera buffer's planes sit in its memory.
//!
//! libcamera hands each frame buffer as one or more dma-bufs ("parts"), each with an fd, an
//! offset and a length. A styx-frame buffer is one memory, so a buffer's planes must be in one
//! dma-buf: either one part holds every plane (libcamera's contiguous case, the planes stacked
//! after each other), or one part per plane, all parts naming the same dma-buf. Anything else
//! is refused with the format's name, because styx-frame would need multi-memory buffers for it.
//!
//! Strides come from libcamera's stream configuration: the luma stride is libcamera's own, and a
//! chroma plane's stride is that luma stride scaled by the plane's bytes per row (styx-format's
//! plane table), which is how libcamera's formats pad their planes.
//!
//! Nothing here touches libcamera, so the planning and its errors are tested in every build.

use std::fmt;

use styx_format::{FourCc, LayoutError, plane_geometry};
use styx_frame::{Layouts, PlaneLayout};

/// One dma-buf part of a libcamera buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Part {
    /// (device, inode) of the dma-buf behind the part's fd. Equal keys are the same memory,
    /// however many fds name it.
    pub(crate) file: (u64, u64),
    /// Where the part starts in its dma-buf.
    pub(crate) offset: usize,
    /// How many bytes the part spans in its dma-buf.
    pub(crate) length: usize,
    /// Whether libcamera set the offset (it does not for unset planes).
    pub(crate) offset_valid: bool,
}

/// A buffer's planes, placed in its memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Each plane's offset, stride and length, in the buffer's memory.
    pub(crate) layouts: Layouts,
    /// Bytes of that dma-buf the frame reaches: map at least this much.
    pub(crate) extent: usize,
    /// Bytes one frame occupies in the memory (the layouts' span).
    pub(crate) span: usize,
}

/// Why a buffer's planes cannot be one styx-frame memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanError {
    /// Compressed formats have no planes to lay out.
    Compressed(FourCc),
    /// Styx's plane table cannot size the format.
    Geometry(FourCc, LayoutError),
    /// libcamera's luma stride is shorter than a row of the format.
    Stride {
        code: FourCc,
        stride: usize,
        row: usize,
    },
    /// A chroma plane's stride is not a whole multiple of the luma stride's ratio.
    ChromaStride { code: FourCc, plane: usize },
    /// A part's offset is not set.
    OffsetUnset { code: FourCc, part: usize },
    /// The buffer has neither one part nor one part per plane.
    PartCount {
        code: FourCc,
        parts: usize,
        planes: usize,
    },
    /// The planes sit in more than one dma-buf.
    SplitMemory {
        code: FourCc,
        parts: usize,
        files: usize,
    },
    /// A plane needs more bytes than its part holds.
    Short {
        code: FourCc,
        plane: usize,
        needed: usize,
        have: usize,
    },
    /// The planes need more bytes than libcamera's frame size for the stream.
    TooLarge {
        code: FourCc,
        needed: usize,
        frame_size: usize,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compressed(code) => write!(f, "{code} is compressed and has no planes here"),
            Self::Geometry(code, err) => write!(f, "{code}: {err}"),
            Self::Stride { code, stride, row } => {
                write!(
                    f,
                    "{code}: libcamera stride {stride} is shorter than a {row}-byte row"
                )
            }
            Self::ChromaStride { code, plane } => write!(
                f,
                "{code}: plane {plane}'s stride is not a whole ratio of libcamera's luma stride"
            ),
            Self::OffsetUnset { code, part } => {
                write!(
                    f,
                    "{code}: dma-buf part {part} has no offset from libcamera"
                )
            }
            Self::PartCount {
                code,
                parts,
                planes,
            } => write!(
                f,
                "{code}: libcamera gave {parts} dma-buf parts for {planes} planes (need 1 or {planes})"
            ),
            Self::SplitMemory { code, parts, files } => write!(
                f,
                "{code}: its {parts} planes sit in {files} different dma-bufs; styx-frame buffers are \
                 one memory each, so this format needs multi-memory buffers, which are not supported yet"
            ),
            Self::Short {
                code,
                plane,
                needed,
                have,
            } => write!(
                f,
                "{code}: plane {plane} needs {needed} bytes but its dma-buf part holds {have}"
            ),
            Self::TooLarge {
                code,
                needed,
                frame_size,
            } => write!(
                f,
                "{code}: planes need {needed} bytes but libcamera's frame size is {frame_size}"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Places `code`'s planes of a `width` x `height` frame in a buffer of `parts`. `luma_stride`
/// and `frame_size` are libcamera's for the stream.
pub(crate) fn plan(
    code: FourCc,
    width: u32,
    height: u32,
    luma_stride: usize,
    frame_size: usize,
    parts: &[Part],
) -> Result<Plan, PlanError> {
    let geometry = plane_geometry(code, width, height).map_err(|err| match err {
        LayoutError::Compressed(code) => PlanError::Compressed(code),
        other => PlanError::Geometry(code, other),
    })?;
    if let Some(part) = parts.iter().position(|part| !part.offset_valid) {
        return Err(PlanError::OffsetUnset { code, part });
    }
    let first_row = geometry[0].row_bytes;
    if luma_stride < first_row {
        return Err(PlanError::Stride {
            code,
            stride: luma_stride,
            row: first_row,
        });
    }

    // Each plane's stride and the bytes it occupies: rows times stride.
    let mut strides = [0usize; styx_format::MAX_PLANES];
    let mut needs = [0usize; styx_format::MAX_PLANES];
    for (index, plane) in geometry.iter().enumerate() {
        let stride = if index == 0 {
            luma_stride
        } else {
            let scaled = luma_stride
                .checked_mul(plane.row_bytes)
                .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?;
            if scaled % first_row != 0 {
                return Err(PlanError::ChromaStride { code, plane: index });
            }
            scaled / first_row
        };
        strides[index] = stride;
        needs[index] = stride
            .checked_mul(plane.rows)
            .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?;
    }
    let planes = geometry.len();

    let distinct = |parts: &[Part]| {
        let mut files: Vec<(u64, u64)> = parts.iter().map(|part| part.file).collect();
        files.sort_unstable();
        files.dedup();
        files.len()
    };

    let (offsets, extent) = if parts.len() == 1 {
        // One part: the planes are stacked from its offset, as libcamera lays them out.
        let part = parts[0];
        let mut offsets = [0usize; styx_format::MAX_PLANES];
        let mut next = part.offset;
        for index in 0..planes {
            offsets[index] = next;
            next = next
                .checked_add(needs[index])
                .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?;
        }
        let end = next - part.offset;
        if end > part.length {
            let plane = (0..planes)
                .find(|&i| offsets[i] - part.offset + needs[i] > part.length)
                .unwrap_or(planes - 1);
            return Err(PlanError::Short {
                code,
                plane,
                needed: offsets[plane] - part.offset + needs[plane],
                have: part.length,
            });
        }
        (offsets, part.offset + part.length)
    } else if parts.len() == planes {
        // One part per plane: every part must be the same dma-buf.
        let files = distinct(parts);
        if files != 1 {
            return Err(PlanError::SplitMemory {
                code,
                parts: parts.len(),
                files,
            });
        }
        let mut offsets = [0usize; styx_format::MAX_PLANES];
        let mut extent = 0usize;
        for (index, part) in parts.iter().enumerate() {
            if needs[index] > part.length {
                return Err(PlanError::Short {
                    code,
                    plane: index,
                    needed: needs[index],
                    have: part.length,
                });
            }
            offsets[index] = part.offset;
            extent = extent.max(
                part.offset
                    .checked_add(part.length)
                    .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?,
            );
        }
        (offsets, extent)
    } else {
        return Err(PlanError::PartCount {
            code,
            parts: parts.len(),
            planes,
        });
    };

    let mut layouts = [PlaneLayout::default(); styx_format::MAX_PLANES];
    for index in 0..planes {
        layouts[index] = PlaneLayout::new(offsets[index], strides[index], needs[index]);
    }
    let layouts = Layouts::new(&layouts[..planes]).ok_or(PlanError::PartCount {
        code,
        parts: parts.len(),
        planes,
    })?;
    let span = layouts
        .span()
        .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?;
    // The planes' bytes, not their place in the memory, against libcamera's frame size.
    let bytes = needs[..planes]
        .iter()
        .try_fold(0usize, |sum, &need| sum.checked_add(need))
        .ok_or(PlanError::Geometry(code, LayoutError::Overflow))?;
    if frame_size != 0 && bytes > frame_size {
        return Err(PlanError::TooLarge {
            code,
            needed: bytes,
            frame_size,
        });
    }
    Ok(Plan {
        layouts,
        extent,
        span,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: (u64, u64) = (0, 7);

    fn part(offset: usize, length: usize) -> Part {
        Part {
            file: FILE,
            offset,
            length,
            offset_valid: true,
        }
    }

    fn planes(plan: &Plan) -> Vec<(usize, usize, usize)> {
        plan.layouts
            .iter()
            .map(|p| (p.offset, p.stride, p.len))
            .collect()
    }

    #[test]
    fn nv12_split_parts_in_one_dmabuf() {
        // 640x480 NV12, luma stride 640: Y then interleaved UV at half the rows.
        let parts = [part(0, 307_200), part(307_200, 153_600)];
        let plan = plan(FourCc::NV12, 640, 480, 640, 460_800, &parts).unwrap();
        assert_eq!(
            planes(&plan),
            vec![(0, 640, 307_200), (307_200, 640, 153_600)]
        );
        assert_eq!(plan.extent, 460_800);
        assert_eq!(plan.span, 460_800);
    }

    #[test]
    fn nv12_single_part_stacks_the_planes() {
        let plan = plan(FourCc::NV12, 640, 480, 640, 460_800, &[part(0, 460_800)]).unwrap();
        assert_eq!(
            planes(&plan),
            vec![(0, 640, 307_200), (307_200, 640, 153_600)]
        );
        assert_eq!(plan.extent, 460_800);
    }

    #[test]
    fn nv21_matches_nv12() {
        let plan = plan(FourCc::NV21, 640, 480, 640, 460_800, &[part(64, 460_800)]).unwrap();
        assert_eq!(
            planes(&plan),
            vec![(64, 640, 307_200), (64 + 307_200, 640, 153_600)]
        );
        assert_eq!(plan.extent, 64 + 460_800);
        assert_eq!(plan.span, 64 + 460_800);
    }

    #[test]
    fn i420_chroma_planes_are_half_stride() {
        let plan = plan(FourCc::I420, 640, 480, 640, 460_800, &[part(0, 460_800)]).unwrap();
        assert_eq!(
            planes(&plan),
            vec![
                (0, 640, 307_200),
                (307_200, 320, 76_800),
                (384_000, 320, 76_800)
            ]
        );
        assert_eq!(plan.span, 460_800);
    }

    #[test]
    fn yuyv_is_one_plane_of_packed_rows() {
        let plan = plan(FourCc::YUYV, 640, 480, 1280, 614_400, &[part(0, 614_400)]).unwrap();
        assert_eq!(planes(&plan), vec![(0, 1280, 614_400)]);
    }

    #[test]
    fn split_dmabufs_are_refused_with_the_format_name() {
        let other = Part {
            file: (0, 9),
            ..part(0, 153_600)
        };
        let err = plan(
            FourCc::NV12,
            640,
            480,
            640,
            460_800,
            &[part(0, 307_200), other],
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::SplitMemory { files: 2, .. }));
        let text = err.to_string();
        assert!(text.starts_with("NV12:"), "{text}");
        assert!(text.contains("multi-memory buffers"), "{text}");
    }

    #[test]
    fn same_dmabuf_under_two_fds_is_one_memory() {
        // Two fds naming one dma-buf carry the same file key: one memory, both planes mapped.
        let parts = [part(0, 307_200), part(307_200, 153_600)];
        assert!(plan(FourCc::NV12, 640, 480, 640, 460_800, &parts).is_ok());
    }

    #[test]
    fn part_count_must_be_one_or_per_plane() {
        let parts = [part(0, 307_200), part(0, 1), part(0, 1), part(0, 1)];
        let err = plan(FourCc::NV12, 640, 480, 640, 460_800, &parts).unwrap_err();
        assert!(matches!(
            err,
            PlanError::PartCount {
                parts: 4,
                planes: 2,
                ..
            }
        ));
    }

    #[test]
    fn short_part_names_the_plane() {
        let err = plan(FourCc::NV12, 640, 480, 640, 460_800, &[part(0, 400_000)]).unwrap_err();
        assert!(matches!(err, PlanError::Short { plane: 1, .. }), "{err}");
        let err = plan(
            FourCc::NV12,
            640,
            480,
            640,
            460_800,
            &[part(0, 307_200), part(307_200, 100)],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlanError::Short {
                plane: 1,
                needed: 153_600,
                have: 100,
                ..
            }
        ));
    }

    #[test]
    fn stride_shorter_than_a_row_is_refused() {
        let err = plan(FourCc::YUYV, 640, 480, 600, 614_400, &[part(0, 614_400)]).unwrap_err();
        assert!(matches!(
            err,
            PlanError::Stride {
                stride: 600,
                row: 1280,
                ..
            }
        ));
    }

    #[test]
    fn compressed_and_unknown_formats_have_no_planes() {
        let err = plan(FourCc::MJPG, 640, 480, 640, 0, &[part(0, 1)]).unwrap_err();
        assert!(matches!(err, PlanError::Compressed(_)), "{err}");
    }

    #[test]
    fn unset_offset_is_refused() {
        let bad = Part {
            offset_valid: false,
            ..part(0, 460_800)
        };
        let err = plan(FourCc::NV12, 640, 480, 640, 460_800, &[bad]).unwrap_err();
        assert!(matches!(err, PlanError::OffsetUnset { part: 0, .. }));
    }

    #[test]
    fn planes_larger_than_libcamera_frame_size_are_refused() {
        let err = plan(FourCc::NV12, 640, 480, 640, 1000, &[part(0, 460_800)]).unwrap_err();
        assert!(matches!(
            err,
            PlanError::TooLarge {
                frame_size: 1000,
                ..
            }
        ));
    }
}

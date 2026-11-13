//! The Styx frame path without `std`: formats looked up, a host frame pool built, a frame written,
//! shared and read back. The firmware image is `src/main.rs`; on a host the same [`run`] is a test
//! here, so `cargo test` checks the logic the image links.
//!
//! The crate asks for no allocator of its own. `styx-frame` allocates once, when its pool is made;
//! the image provides the global allocator.

#![no_std]

use styx_format::{FourCc, MAX_PLANES, MediaFormat, Resolution};
use styx_frame::{AcquireError, BufferSize, FrameEntry, FramePool, PoolError, packed_layouts};

/// The frame's size: NV12 at QQVGA, the smallest format the pool is built for.
const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
/// Buffers in the pool: one frame written and shared, one free for the next.
const BUFFERS: usize = 2;
/// GPU slack per buffer (the pool's rule: asked for once, frames never use it).
const GPU_SLACK: usize = 256;

/// What [`run`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Formats of the look-up list that the format table knows (of [`LOOKED_UP`]).
    pub formats_known: usize,
    /// Planes of the frame (NV12: 2).
    pub planes: usize,
    /// Bytes one frame's planes take, from offset 0.
    pub frame_bytes: usize,
    /// Leases on the frame after sharing (2).
    pub holders: u32,
    /// Bytes read back and checked.
    pub bytes_checked: usize,
}

/// Why [`run`] stopped. Each variant names the step that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeError {
    /// A format of the look-up list is missing from the table.
    UnknownFormat,
    /// The frame's plane layout could not be made.
    Layout,
    /// The frame pool could not be built.
    Pool(PoolError),
    /// The pool gave no buffer for the frame.
    Acquire(AcquireError),
    /// The frame had no writable planes.
    Write,
    /// A read-back byte differed from what was written.
    Mismatch,
    /// The frame had no readable planes.
    Read,
}

/// The formats the look-up checks: planar YUV, packed YUV, Bayer and a grey format.
pub const LOOKED_UP: [FourCc; 4] = [FourCc::NV12, FourCc::YUYV, FourCc::RGGB, FourCc::GREY];

/// The byte written at `col` of `row` in plane `plane`: distinct per plane, row and column.
fn pattern(plane: usize, row: usize, col: usize) -> u8 {
    (plane * 97 + row * 7 + col) as u8
}

/// Looks up formats, builds a two-buffer host pool, acquires a frame, writes it, shares it and
/// reads both leases back, checking every byte of the planes.
pub fn run() -> Result<Report, SmokeError> {
    let formats_known = LOOKED_UP
        .iter()
        .filter(|code| code.info().is_some())
        .count();
    if formats_known != LOOKED_UP.len() {
        return Err(SmokeError::UnknownFormat);
    }

    let code = FourCc::NV12;
    let layouts =
        packed_layouts(code, WIDTH, HEIGHT, None, None).map_err(|_| SmokeError::Layout)?;
    let frame_bytes = layouts.span().ok_or(SmokeError::Layout)?;
    let planes = layouts.len();
    // Rows per plane: a chroma plane of NV12 has half as many as the luma plane.
    let mut rows = [0usize; MAX_PLANES];
    for (row_count, plane) in rows.iter_mut().zip(layouts.iter()) {
        *row_count = plane.len / plane.stride;
    }
    let size = BufferSize::new(frame_bytes, GPU_SLACK);
    let pool: FramePool = FramePool::host(BUFFERS, size).map_err(SmokeError::Pool)?;

    let resolution = Resolution::new(WIDTH, HEIGHT).ok_or(SmokeError::Layout)?;
    let format = MediaFormat::with_defaults(code, resolution);
    let entry = FrameEntry::new(1, 1_000_000, format, layouts);
    let mut lease = pool.acquire(entry).map_err(SmokeError::Acquire)?;

    {
        let mut planes_mut = lease.write().ok_or(SmokeError::Write)?;
        for (plane, &plane_rows) in rows.iter().enumerate().take(planes) {
            let mut target = planes_mut.plane(plane).ok_or(SmokeError::Write)?;
            for row in 0..plane_rows {
                let bytes = target
                    .row_bytes_mut(row, WIDTH as usize)
                    .ok_or(SmokeError::Write)?;
                for (col, byte) in bytes.iter_mut().enumerate() {
                    *byte = pattern(plane, row, col);
                }
            }
        }
    }

    let shared = lease.share();
    let holders = shared.holders();
    let mut bytes_checked = 0;
    let read = shared.read().ok_or(SmokeError::Read)?;
    for (plane, &plane_rows) in rows.iter().enumerate().take(planes) {
        let data = read.plane(plane).ok_or(SmokeError::Read)?;
        for row in 0..plane_rows {
            let bytes = data
                .row_bytes(row, WIDTH as usize)
                .ok_or(SmokeError::Read)?;
            for (col, byte) in bytes.iter().enumerate() {
                if *byte != pattern(plane, row, col) {
                    return Err(SmokeError::Mismatch);
                }
            }
            bytes_checked += bytes.len();
        }
    }

    Ok(Report {
        formats_known,
        planes,
        frame_bytes,
        holders,
        bytes_checked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let report = run().expect("smoke run");
        assert_eq!(report.formats_known, LOOKED_UP.len());
        assert_eq!(report.planes, 2);
        assert_eq!(report.frame_bytes, 160 * 120 * 3 / 2);
        assert_eq!(report.holders, 2);
        assert_eq!(report.bytes_checked, 160 * 120 * 3 / 2);
    }
}

//! The planes of a frame: visible bytes per row, rows, and with alignment, stride and length.

use core::fmt;
use core::ops::Deref;

use crate::{ChromaSubsampling, FourCc, Storage};

/// The most planes a format has.
pub const MAX_PLANES: usize = 3;

/// Up to [`MAX_PLANES`] values, one per plane, without allocating. Dereferences to a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planes<T> {
    items: [T; MAX_PLANES],
    count: u8,
}

impl<T: Copy + Default> Planes<T> {
    fn from_slice(items: &[T]) -> Self {
        let mut out = [T::default(); MAX_PLANES];
        out[..items.len()].copy_from_slice(items);
        Self {
            items: out,
            count: items.len() as u8,
        }
    }
}

impl<T> Deref for Planes<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items[..usize::from(self.count)]
    }
}

/// The visible part of one plane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PlaneGeometry {
    /// Bytes of a row's samples.
    pub row_bytes: usize,
    /// Rows.
    pub rows: usize,
}

/// A plane's memory: bytes from one row to the next, and bytes in all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PlaneSize {
    pub stride: usize,
    pub len: usize,
}

/// Why a frame's planes cannot be laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// Styx does not know the format.
    UnknownFormat(FourCc),
    /// A compressed format has no planes to lay out.
    Compressed(FourCc),
    /// An alignment that is zero or not a power of two.
    InvalidAlignment(usize),
    /// The sizes do not fit in `usize`.
    Overflow,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat(code) => write!(f, "unknown format {code}"),
            Self::Compressed(code) => write!(f, "{code} is compressed: no planes"),
            Self::InvalidAlignment(a) => write!(f, "alignment {a} is not a power of two"),
            Self::Overflow => f.write_str("plane size overflows"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for LayoutError {}

/// The visible bytes and rows of each plane of a `width` x `height` frame of `code`.
///
/// Chroma planes of odd sizes round up: a 4:2:0 frame of 3 rows has 2 chroma rows. A
/// semi-planar chroma row is as many bytes as the luma row (4:2:0, 4:2:2) or twice (4:4:4).
pub fn plane_geometry(
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<Planes<PlaneGeometry>, LayoutError> {
    let info = code.info().ok_or(LayoutError::UnknownFormat(code))?;
    let (w, h) = (width as usize, height as usize);
    let g = |row_bytes: usize, rows: usize| PlaneGeometry { row_bytes, rows };
    let planes = match (info.storage, info.subsampling) {
        (Storage::Compressed, _) => return Err(LayoutError::Compressed(code)),
        (Storage::Packed | Storage::RawBayer, _) => {
            let row = info.first_plane_row_bytes(w).ok_or(LayoutError::Overflow)?;
            Planes::from_slice(&[g(row, h)])
        }
        (Storage::SemiPlanar, Some(ChromaSubsampling::Cs420)) => {
            Planes::from_slice(&[g(w, h), g(w, h.div_ceil(2))])
        }
        (Storage::SemiPlanar, Some(ChromaSubsampling::Cs422)) => {
            Planes::from_slice(&[g(w, h), g(w, h)])
        }
        (Storage::SemiPlanar, Some(ChromaSubsampling::Cs444)) => {
            let chroma = w.checked_mul(2).ok_or(LayoutError::Overflow)?;
            Planes::from_slice(&[g(w, h), g(chroma, h)])
        }
        (Storage::Planar, Some(ChromaSubsampling::Cs420)) => {
            let c = g(w.div_ceil(2), h.div_ceil(2));
            Planes::from_slice(&[g(w, h), c, c])
        }
        (Storage::Planar, Some(ChromaSubsampling::Cs422)) => {
            let c = g(w.div_ceil(2), h);
            Planes::from_slice(&[g(w, h), c, c])
        }
        (Storage::Planar, Some(ChromaSubsampling::Cs444)) => Planes::from_slice(&[g(w, h); 3]),
        (Storage::SemiPlanar | Storage::Planar, None) => Planes::from_slice(&[g(w, h)]),
    };
    Ok(planes)
}

fn align_up(value: usize, alignment: Option<usize>) -> Result<usize, LayoutError> {
    let Some(a) = alignment else {
        return Ok(value);
    };
    if !a.is_power_of_two() {
        return Err(LayoutError::InvalidAlignment(a));
    }
    value
        .checked_add(a - 1)
        .map(|v| v & !(a - 1))
        .ok_or(LayoutError::Overflow)
}

/// The stride and length of each plane of a `width` x `height` frame of `code`, each plane in
/// its own memory: rows padded to `stride_alignment` bytes, planes to `plane_alignment`.
///
/// ```
/// use styx_format::{FourCc, PlaneSize, plane_sizes};
///
/// let planes = plane_sizes(FourCc::NV12, 640, 480, Some(64), None).unwrap();
/// assert_eq!(&planes[..], &[
///     PlaneSize { stride: 640, len: 307_200 },
///     PlaneSize { stride: 640, len: 153_600 },
/// ]);
/// ```
pub fn plane_sizes(
    code: FourCc,
    width: u32,
    height: u32,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<Planes<PlaneSize>, LayoutError> {
    let geometry = plane_geometry(code, width, height)?;
    let mut sizes = [PlaneSize::default(); MAX_PLANES];
    for (size, plane) in sizes.iter_mut().zip(geometry.iter()) {
        let stride = align_up(plane.row_bytes, stride_alignment)?;
        let bytes = stride
            .checked_mul(plane.rows)
            .ok_or(LayoutError::Overflow)?;
        *size = PlaneSize {
            stride,
            len: align_up(bytes, plane_alignment)?,
        };
    }
    Ok(Planes::from_slice(&sizes[..geometry.len()]))
}

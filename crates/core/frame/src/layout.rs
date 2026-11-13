//! Where each plane sits in its buffer.

use core::ops::Deref;

use styx_format::{FourCc, LayoutError, MAX_PLANES, plane_sizes};

/// One plane in a buffer: its first byte, bytes from one row to the next, and its length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PlaneLayout {
    pub offset: usize,
    pub stride: usize,
    pub len: usize,
}

impl PlaneLayout {
    pub const fn new(offset: usize, stride: usize, len: usize) -> Self {
        Self {
            offset,
            stride,
            len,
        }
    }

    /// One past the last byte, `None` on overflow.
    pub const fn end(&self) -> Option<usize> {
        self.offset.checked_add(self.len)
    }
}

/// Up to [`MAX_PLANES`] plane layouts, inline. Dereferences to a slice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Layouts {
    items: [PlaneLayout; MAX_PLANES],
    count: u8,
}

impl Layouts {
    /// No planes.
    pub const EMPTY: Self = Self {
        items: [PlaneLayout::new(0, 0, 0); MAX_PLANES],
        count: 0,
    };

    /// `None` when there are more than [`MAX_PLANES`].
    pub fn new(planes: &[PlaneLayout]) -> Option<Self> {
        if planes.len() > MAX_PLANES {
            return None;
        }
        let mut items = [PlaneLayout::default(); MAX_PLANES];
        items[..planes.len()].copy_from_slice(planes);
        Some(Self {
            items,
            count: planes.len() as u8,
        })
    }

    /// One past the last byte of any plane: how much of the buffer the frame uses.
    pub fn span(&self) -> Option<usize> {
        self.iter()
            .try_fold(0usize, |end, plane| Some(end.max(plane.end()?)))
    }
}

impl Deref for Layouts {
    type Target = [PlaneLayout];

    fn deref(&self) -> &[PlaneLayout] {
        &self.items[..usize::from(self.count)]
    }
}

/// The planes of `code` at `width` x `height` one after another from offset 0, with the format
/// crate's sizes ([`styx_format::plane_sizes`]).
pub fn packed_layouts(
    code: FourCc,
    width: u32,
    height: u32,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<Layouts, LayoutError> {
    let sizes = plane_sizes(code, width, height, stride_alignment, plane_alignment)?;
    let mut items = [PlaneLayout::default(); MAX_PLANES];
    let mut offset = 0usize;
    for (item, size) in items.iter_mut().zip(sizes.iter()) {
        *item = PlaneLayout::new(offset, size.stride, size.len);
        offset = offset.checked_add(size.len).ok_or(LayoutError::Overflow)?;
    }
    Ok(Layouts {
        items,
        count: sizes.len() as u8,
    })
}

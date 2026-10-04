//! Borrowed plane and visible-row views.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane<'a> {
    pub(super) data: &'a [u8],
    pub(super) stride: usize,
}

#[derive(Debug)]
pub struct PlaneMut<'a> {
    pub(super) data: &'a mut [u8],
    pub(super) stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRows<'a> {
    pub(super) data: &'a [u8],
    pub(super) stride: usize,
    pub(super) row_bytes: usize,
    pub(super) rows: usize,
}

#[derive(Debug)]
pub struct VisibleRowsMut<'a> {
    pub(super) data: &'a mut [u8],
    pub(super) stride: usize,
    pub(super) row_bytes: usize,
    pub(super) rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRow<'a> {
    pub(super) data: &'a [u8],
}

#[derive(Debug)]
pub struct VisibleRowMut<'a> {
    pub(super) data: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePlaneShape {
    pub width: usize,
    pub height: usize,
    pub row_bytes: usize,
    pub stride: usize,
    pub offset: usize,
    pub len: usize,
}

impl<'a> Plane<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> PlaneMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> VisibleRows<'a> {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn visible_len(&self) -> usize {
        self.row_bytes.saturating_mul(self.rows)
    }

    pub fn row(&self, index: usize) -> Option<VisibleRow<'a>> {
        if index >= self.rows {
            return None;
        }
        let start = index.checked_mul(self.stride)?;
        let end = start.checked_add(self.row_bytes)?;
        Some(VisibleRow {
            data: self.data.get(start..end)?,
        })
    }

    pub fn iter(&self) -> VisibleRowsIter<'a> {
        VisibleRowsIter {
            rows: *self,
            index: 0,
        }
    }
}

impl<'a> IntoIterator for VisibleRows<'a> {
    type Item = VisibleRow<'a>;
    type IntoIter = VisibleRowsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        VisibleRowsIter {
            rows: self,
            index: 0,
        }
    }
}

pub struct VisibleRowsIter<'a> {
    pub(super) rows: VisibleRows<'a>,
    pub(super) index: usize,
}

impl<'a> Iterator for VisibleRowsIter<'a> {
    type Item = VisibleRow<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let row = self.rows.row(self.index)?;
        self.index += 1;
        Some(row)
    }
}

impl<'a> VisibleRowsMut<'a> {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn visible_len(&self) -> usize {
        self.row_bytes.saturating_mul(self.rows)
    }

    pub fn for_each_row_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(usize, VisibleRowMut<'_>),
    {
        for index in 0..self.rows {
            let start = index * self.stride;
            let end = start + self.row_bytes;
            if let Some(data) = self.data.get_mut(start..end) {
                f(index, VisibleRowMut { data });
            }
        }
    }
}

impl<'a> VisibleRow<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

impl<'a> VisibleRowMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }
}

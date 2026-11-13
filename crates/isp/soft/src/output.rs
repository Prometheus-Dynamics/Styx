//! Output buffers and their validation and splitting into row bands.

use alloc::format;

use crate::IspError;

/// Output scale.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    #[default]
    Full,
    /// Half width and height: each 2x2 quad of the mosaic becomes one pixel (red, mean green,
    /// blue), with no demosaic. The fast path to a smaller image.
    Half,
}

/// Where the processed frame goes. Strides are in bytes.
#[derive(Debug)]
pub enum OutputBuffers<'a> {
    /// Packed 8-bit R, G, B.
    Rgb24 { data: &'a mut [u8], stride: usize },
    /// 8-bit luma plane and interleaved half-resolution Cb/Cr plane.
    Nv12 {
        y: &'a mut [u8],
        y_stride: usize,
        uv: &'a mut [u8],
        uv_stride: usize,
    },
    /// Planar 4:2:0: luma, Cb, Cr.
    I420 {
        y: &'a mut [u8],
        y_stride: usize,
        u: &'a mut [u8],
        u_stride: usize,
        v: &'a mut [u8],
        v_stride: usize,
    },
    /// 8-bit luma only (for full scale computed straight from the mosaic, without demosaic
    /// or colour correction).
    Luma { data: &'a mut [u8], stride: usize },
}

/// The kind of output, without buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Rgb24,
    Nv12,
    I420,
    Luma,
}

/// A plane `name` of `len` bytes holds `rows` rows of `row` bytes at `stride`.
pub fn check(
    name: &str,
    len: usize,
    stride: usize,
    row: usize,
    rows: usize,
) -> Result<(), IspError> {
    let need = if rows == 0 {
        0
    } else {
        stride * (rows - 1) + row
    };
    if stride < row || len < need {
        return Err(IspError::OutputTooSmall(format!(
            "{name}: {len} bytes with stride {stride}, need stride >= {row} and {need} bytes"
        )));
    }
    Ok(())
}

impl<'a> OutputBuffers<'a> {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Rgb24 { .. } => Kind::Rgb24,
            Self::Nv12 { .. } => Kind::Nv12,
            Self::I420 { .. } => Kind::I420,
            Self::Luma { .. } => Kind::Luma,
        }
    }

    /// Check the buffers hold a `w` x `h` image.
    pub(crate) fn validate(&self, w: usize, h: usize) -> Result<(), IspError> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        match self {
            Self::Rgb24 { data, stride } => check("rgb", data.len(), *stride, 3 * w, h),
            Self::Luma { data, stride } => check("luma", data.len(), *stride, w, h),
            Self::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => {
                check("y", y.len(), *y_stride, w, h)?;
                check("uv", uv.len(), *uv_stride, 2 * cw, ch)
            }
            Self::I420 {
                y,
                y_stride,
                u,
                u_stride,
                v,
                v_stride,
            } => {
                check("y", y.len(), *y_stride, w, h)?;
                check("u", u.len(), *u_stride, cw, ch)?;
                check("v", v.len(), *v_stride, cw, ch)
            }
        }
    }

    /// This output's first `rows` rows (`rows` even for 4:2:0), and the rest.
    fn split_rows(self, rows: usize) -> (Self, Self) {
        fn cut(s: &mut [u8], at: usize) -> (&mut [u8], &mut [u8]) {
            let at = at.min(s.len());
            s.split_at_mut(at)
        }
        match self {
            Self::Rgb24 { data, stride } => {
                let (a, b) = cut(data, stride * rows);
                (
                    Self::Rgb24 { data: a, stride },
                    Self::Rgb24 { data: b, stride },
                )
            }
            Self::Luma { data, stride } => {
                let (a, b) = cut(data, stride * rows);
                (
                    Self::Luma { data: a, stride },
                    Self::Luma { data: b, stride },
                )
            }
            Self::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => {
                let (y0, y1) = cut(y, y_stride * rows);
                let (uv0, uv1) = cut(uv, uv_stride * (rows / 2));
                (
                    Self::Nv12 {
                        y: y0,
                        y_stride,
                        uv: uv0,
                        uv_stride,
                    },
                    Self::Nv12 {
                        y: y1,
                        y_stride,
                        uv: uv1,
                        uv_stride,
                    },
                )
            }
            Self::I420 {
                y,
                y_stride,
                u,
                u_stride,
                v,
                v_stride,
            } => {
                let (y0, y1) = cut(y, y_stride * rows);
                let (u0, u1) = cut(u, u_stride * (rows / 2));
                let (v0, v1) = cut(v, v_stride * (rows / 2));
                (
                    Self::I420 {
                        y: y0,
                        y_stride,
                        u: u0,
                        u_stride,
                        v: v0,
                        v_stride,
                    },
                    Self::I420 {
                        y: y1,
                        y_stride,
                        u: u1,
                        u_stride,
                        v: v1,
                        v_stride,
                    },
                )
            }
        }
    }

    /// Bands of `band` rows each of an output of `h` rows (the last may be shorter; `band` even),
    /// split off one at a time, so a frame allocates nothing for them.
    pub(crate) fn into_bands(self, band: usize, h: usize) -> Bands<'a> {
        debug_assert!(band.is_multiple_of(2) || band >= h);
        Bands {
            rest: Some(self),
            band,
            left: h.div_ceil(band.max(1)),
        }
    }
}

/// The bands of an output: see [`OutputBuffers::into_bands`].
pub(crate) struct Bands<'a> {
    rest: Option<OutputBuffers<'a>>,
    band: usize,
    left: usize,
}

impl<'a> Iterator for Bands<'a> {
    type Item = OutputBuffers<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let rest = self.rest.take()?;
        if self.left == 0 {
            return Some(rest);
        }
        let (head, tail) = rest.split_rows(self.band);
        self.rest = Some(tail);
        Some(head)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Bands<'_> {}

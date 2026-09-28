//! Output buffers and their validation and splitting into row bands.

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

fn check(name: &str, len: usize, stride: usize, row: usize, rows: usize) -> Result<(), IspError> {
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

    /// Split into bands of `band` rows each (the last may be shorter; `band` even).
    pub(crate) fn into_bands(self, band: usize, h: usize) -> Vec<OutputBuffers<'a>> {
        debug_assert!(band.is_multiple_of(2) || band >= h);
        let starts: Vec<usize> = (0..h).step_by(band.max(1)).collect();
        fn split(mut s: &mut [u8], stride: usize, rows: usize, n: usize) -> Vec<&mut [u8]> {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                if i + 1 == n {
                    out.push(std::mem::take(&mut s));
                } else {
                    let t = std::mem::take(&mut s);
                    let at = (stride * rows).min(t.len());
                    let (a, b) = t.split_at_mut(at);
                    out.push(a);
                    s = b;
                }
            }
            out
        }
        let n = starts.len();
        match self {
            Self::Rgb24 { data, stride } => split(data, stride, band, n)
                .into_iter()
                .map(|data| OutputBuffers::Rgb24 { data, stride })
                .collect(),
            Self::Luma { data, stride } => split(data, stride, band, n)
                .into_iter()
                .map(|data| OutputBuffers::Luma { data, stride })
                .collect(),
            Self::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => split(y, y_stride, band, n)
                .into_iter()
                .zip(split(uv, uv_stride, band / 2, n))
                .map(|(y, uv)| OutputBuffers::Nv12 {
                    y,
                    y_stride,
                    uv,
                    uv_stride,
                })
                .collect(),
            Self::I420 {
                y,
                y_stride,
                u,
                u_stride,
                v,
                v_stride,
            } => split(y, y_stride, band, n)
                .into_iter()
                .zip(split(u, u_stride, band / 2, n))
                .zip(split(v, v_stride, band / 2, n))
                .map(|((y, u), v)| OutputBuffers::I420 {
                    y,
                    y_stride,
                    u,
                    u_stride,
                    v,
                    v_stride,
                })
                .collect(),
        }
    }
}

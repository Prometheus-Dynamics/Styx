//! Output kinds and the plane layout the GPU writes them in.

use styx_softisp::{IspError, OutputBuffers};

/// The kind of output, without buffers (as [`OutputBuffers`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputKind {
    Rgb24,
    Nv12,
    I420,
    Luma,
}

impl OutputKind {
    pub fn of(out: &OutputBuffers<'_>) -> Self {
        match out {
            OutputBuffers::Rgb24 { .. } => Self::Rgb24,
            OutputBuffers::Nv12 { .. } => Self::Nv12,
            OutputBuffers::I420 { .. } => Self::I420,
            OutputBuffers::Luma { .. } => Self::Luma,
        }
    }

    pub(crate) fn code(self) -> u32 {
        match self {
            Self::Rgb24 => 0,
            Self::Nv12 => 1,
            Self::I420 => 2,
            Self::Luma => 3,
        }
    }

    pub fn planes(self) -> usize {
        match self {
            Self::Rgb24 | Self::Luma => 1,
            Self::Nv12 => 2,
            Self::I420 => 3,
        }
    }
}

/// One plane of a GPU output buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    pub offset: usize,
    pub stride: usize,
    /// Bytes of each row that hold pixels.
    pub row: usize,
    pub rows: usize,
}

/// Where the planes of an output image lie in its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub kind: OutputKind,
    pub width: usize,
    pub height: usize,
    /// `kind.planes()` of them are used.
    pub planes: [Plane; 3],
    /// Bytes in all.
    pub size: usize,
}

impl Layout {
    /// Planes one after another, each row starting on a multiple of `align` bytes.
    pub fn new(kind: OutputKind, width: usize, height: usize, align: usize) -> Self {
        let align = align.max(1);
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let rows: &[(usize, usize)] = match kind {
            OutputKind::Rgb24 => &[(3 * width, height)],
            OutputKind::Luma => &[(width, height)],
            OutputKind::Nv12 => &[(width, height), (2 * cw, ch)],
            OutputKind::I420 => &[(width, height), (cw, ch), (cw, ch)],
        };
        let mut planes = [Plane::default(); 3];
        let mut at = 0;
        for (p, &(row, n)) in planes.iter_mut().zip(rows) {
            let stride = row.next_multiple_of(align);
            *p = Plane {
                offset: at,
                stride,
                row,
                rows: n,
            };
            at += (stride * n).next_multiple_of(align.max(4));
        }
        Self {
            kind,
            width,
            height,
            planes,
            size: at,
        }
    }

    /// The used planes.
    pub fn used(&self) -> &[Plane] {
        &self.planes[..self.kind.planes()]
    }
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

/// The caller's planes (data, stride), checked against `layout` as `styx-softisp` checks them.
pub(crate) fn destination<'a, 'b>(
    out: &'b mut OutputBuffers<'a>,
    layout: &Layout,
) -> Result<Vec<(&'b mut [u8], usize)>, IspError> {
    let names = ["y", "u", "v"];
    let planes: Vec<(&'b mut [u8], usize)> = match out {
        OutputBuffers::Rgb24 { data, stride } | OutputBuffers::Luma { data, stride } => {
            vec![(&mut **data, *stride)]
        }
        OutputBuffers::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } => vec![(&mut **y, *y_stride), (&mut **uv, *uv_stride)],
        OutputBuffers::I420 {
            y,
            y_stride,
            u,
            u_stride,
            v,
            v_stride,
        } => vec![
            (&mut **y, *y_stride),
            (&mut **u, *u_stride),
            (&mut **v, *v_stride),
        ],
    };
    for (k, ((data, stride), p)) in planes.iter().zip(layout.used()).enumerate() {
        let name = match layout.kind {
            OutputKind::Rgb24 => "rgb",
            OutputKind::Luma => "luma",
            OutputKind::Nv12 if k == 1 => "uv",
            _ => names[k],
        };
        check(name, data.len(), *stride, p.row, p.rows)?;
    }
    Ok(planes)
}

/// Copy the planes of `src` (laid out as `layout`) into the caller's planes.
pub(crate) fn copy_out(src: &[u8], layout: &Layout, dst: Vec<(&mut [u8], usize)>) {
    for (p, (data, stride)) in layout.used().iter().zip(dst) {
        if stride == p.stride && p.stride == p.row {
            let n = p.row * p.rows;
            data[..n].copy_from_slice(&src[p.offset..][..n]);
            continue;
        }
        for r in 0..p.rows {
            data[r * stride..][..p.row].copy_from_slice(&src[p.offset + r * p.stride..][..p.row]);
        }
    }
}

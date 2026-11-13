//! Raw YUV converters: YUYV, NV12 and I420 to RGB24, and YUYV and NV12 to luma (GREY).
//!
//! Each converter reads its input planes with their strides (a packed plane whose length is
//! exactly its rows is read tightly; a longer one by its stride), writes the output rows of a
//! packed output lease, and allocates nothing. Scalar rows: see [`crate::yuv`].

use styx_codec_api::{Backend, Codec, CodecDescriptor, CodecError, CodecKind, Converter};
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_frame::{CpuRead, FrameEntry, FrameLease, Plane, packed_layouts};

use crate::yuv::{YuvMatrix, i420_rgb_row, nv12_rgb_row, yuyv_rgb_row};

/// Which source format a converter reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Yuyv,
    Nv12,
    I420,
}

/// What it writes: RGB24, or the luma of the frame as GREY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Rgb24,
    Grey,
}

/// One raw converter: a source format and a target.
#[derive(Debug)]
pub struct RawConverter {
    descriptor: CodecDescriptor,
    source: Source,
    target: Target,
}

impl RawConverter {
    /// The descriptor this converter carries.
    pub fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    /// `None` for a pair with no converter here (I420 to GREY, and so on).
    pub fn new(source: Source, target: Target) -> Option<Self> {
        let (input, name) = match (source, target) {
            (Source::Yuyv, Target::Rgb24) => (FourCc::YUYV, "yuyv-to-rgb24"),
            (Source::Yuyv, Target::Grey) => (FourCc::YUYV, "yuyv-to-grey"),
            (Source::Nv12, Target::Rgb24) => (FourCc::NV12, "nv12-to-rgb24"),
            (Source::Nv12, Target::Grey) => (FourCc::NV12, "nv12-to-grey"),
            (Source::I420, Target::Rgb24) => (FourCc::I420, "i420-to-rgb24"),
            (Source::I420, Target::Grey) => return None,
        };
        Some(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Converter,
                input,
                output: output_code(target),
                backend: Backend::Native,
                name,
            },
            source,
            target,
        })
    }
}

fn output_code(target: Target) -> FourCc {
    match target {
        Target::Rgb24 => FourCc::RG24,
        Target::Grey => FourCc::GREY,
    }
}

fn bad(msg: &str) -> CodecError {
    CodecError::Rejected(msg.to_string())
}

/// The plane's bytes and stride, widened to the packed minimum `min_stride`. A plane whose
/// length is exactly `min_stride * rows` is read tightly even when its stride says otherwise.
fn plane_view<'a>(
    plane: Plane<'a>,
    min_stride: usize,
    rows: usize,
) -> Result<(&'a [u8], usize), CodecError> {
    let mut stride = plane.stride.max(min_stride);
    if min_stride.checked_mul(rows) == Some(plane.data.len()) {
        stride = min_stride;
    }
    let need = stride
        .checked_mul(rows)
        .ok_or_else(|| bad("plane size overflows"))?;
    if plane.data.len() < need {
        return Err(bad("plane buffer too short"));
    }
    Ok((&plane.data[..need], stride))
}

/// Row `y` of a plane view, from its start to the end of the buffer (the kernels read whole
/// groups, which may run into the next row's bytes, as dev's do).
fn row_from(data: &[u8], stride: usize, y: usize) -> &[u8] {
    &data[y * stride..]
}

impl<M> Codec<M> for RawConverter {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Converter<M> for RawConverter {
    fn convert(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let fmt = *input.format();
        if fmt.code != self.descriptor.input {
            return Err(bad(&format!(
                "expected {}, got {}",
                self.descriptor.input, fmt.code
            )));
        }
        let (w, h) = (
            fmt.resolution.width.get() as usize,
            fmt.resolution.height.get() as usize,
        );
        let color = fmt.color;
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("input frame is not readable".into()))?;
        let rgb = self.target == Target::Rgb24;
        let matrix = YuvMatrix::for_color(color);

        // Output first: sized and entered, then its rows are written.
        begin_output(
            input,
            output,
            self.descriptor.output,
            w,
            h,
            color,
            fmt.range,
        )?;
        let mut planes = output
            .write()
            .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
        let out = planes
            .plane(0)
            .ok_or_else(|| CodecError::Failed("output has no plane".into()))?;
        let out_bpp = if rgb { 3 } else { 1 };
        let out_stride = w * out_bpp;
        let dst = out.data;

        match self.source {
            Source::Yuyv => {
                let p = single_plane(&read)?;
                let (src, stride) = plane_view(p, w * 2, h)?;
                for y in 0..h {
                    let line = row_from(src, stride, y);
                    let row = &mut dst[y * out_stride..][..out_stride];
                    if rgb {
                        // An odd last pixel needs its group's V byte; a row without it leaves
                        // that pixel, as dev's converter does.
                        let groups = w.div_ceil(2) * 4;
                        let width = if line.len() >= groups { w } else { w & !1 };
                        yuyv_rgb_row(line, row, width, &matrix);
                    } else {
                        for (d, s) in row[..w].iter_mut().zip(line.as_chunks::<2>().0) {
                            *d = s[0];
                        }
                    }
                }
            }
            Source::Nv12 => {
                let (y_view, uv_view) = nv12_views(&read, w, h)?;
                for y in 0..h {
                    let row = &mut dst[y * out_stride..][..out_stride];
                    let yl = row_from(y_view.0, y_view.1, y);
                    if rgb {
                        let uvl = row_from(uv_view.0, uv_view.1, y / 2);
                        nv12_rgb_row(yl, uvl, row, w, &matrix);
                    } else {
                        row[..w].copy_from_slice(&yl[..w]);
                    }
                }
            }
            Source::I420 => {
                let (yv, uv, vv) = i420_views(&read, w, h)?;
                for y in 0..h {
                    let row = &mut dst[y * out_stride..][..out_stride];
                    let yl = row_from(yv.0, yv.1, y);
                    let ul = row_from(uv.0, uv.1, y / 2);
                    let vl = row_from(vv.0, vv.1, y / 2);
                    i420_rgb_row(yl, ul, vl, row, w, &matrix);
                }
            }
        }
        Ok(())
    }
}

/// Sizes the output lease for the converter's target and enters it with the input's identity.
fn begin_output<M>(
    input: &FrameLease<M>,
    output: &mut FrameLease<M>,
    code: FourCc,
    w: usize,
    h: usize,
    color: ColorSpace,
    range: ColorRange,
) -> Result<(), CodecError> {
    let (Ok(w32), Ok(h32)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(bad("frame size does not fit"));
    };
    let res = Resolution::new(w32, h32).ok_or_else(|| bad("empty frame"))?;
    let layouts = packed_layouts(code, w32, h32, None, None).map_err(|e| bad(&e.to_string()))?;
    output
        .set_entry(FrameEntry::new(
            input.id(),
            input.timestamp_ns(),
            MediaFormat::new(code, res, color, range),
            layouts,
        ))
        .map_err(|e| CodecError::Rejected(format!("output lease: {e}")))
}

fn single_plane<'a, M>(read: &CpuRead<'a, M>) -> Result<Plane<'a>, CodecError> {
    read.plane(0).ok_or_else(|| bad("frame has no plane"))
}

/// Y and UV views of an NV12 frame: two planes, or one plane holding Y then the UV rows.
#[allow(clippy::type_complexity)]
fn nv12_views<'a, M>(
    read: &CpuRead<'a, M>,
    w: usize,
    h: usize,
) -> Result<((&'a [u8], usize), (&'a [u8], usize)), CodecError> {
    let ch = h.div_ceil(2);
    if read.planes() >= 2 {
        let y = plane_view(single_plane(read)?, w, h)?;
        let uv = plane_view(
            read.plane(1).ok_or_else(|| bad("nv12 missing UV plane"))?,
            w.div_ceil(2) * 2,
            ch,
        )?;
        Ok((y, uv))
    } else {
        let p = single_plane(read)?;
        // Chroma rows of an odd width hold one byte more than the luma rows.
        let stride = p.stride.max(w).max(w.div_ceil(2) * 2);
        let y_len = stride
            .checked_mul(h)
            .ok_or_else(|| bad("nv12 size overflows"))?;
        let uv_len = stride
            .checked_mul(ch)
            .ok_or_else(|| bad("nv12 size overflows"))?;
        let total = y_len
            .checked_add(uv_len)
            .ok_or_else(|| bad("nv12 size overflows"))?;
        if p.data.len() < total {
            return Err(bad("nv12 plane buffer too short"));
        }
        Ok(((&p.data[..y_len], stride), (&p.data[y_len..total], stride)))
    }
}

/// Y, U and V views of an I420 frame: three planes.
#[allow(clippy::type_complexity)]
fn i420_views<'a, M>(
    read: &CpuRead<'a, M>,
    w: usize,
    h: usize,
) -> Result<((&'a [u8], usize), (&'a [u8], usize), (&'a [u8], usize)), CodecError> {
    if read.planes() < 3 {
        return Err(bad("i420 frame needs three planes"));
    }
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let y = plane_view(read.plane(0).ok_or_else(|| bad("i420 missing Y"))?, w, h)?;
    let u = plane_view(read.plane(1).ok_or_else(|| bad("i420 missing U"))?, cw, ch)?;
    let v = plane_view(read.plane(2).ok_or_else(|| bad("i420 missing V"))?, cw, ch)?;
    Ok((y, u, v))
}

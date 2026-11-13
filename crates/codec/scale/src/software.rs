//! The software converter: resize, and channel or chroma-order conversion, on the CPU.

use styx_codec_api::{Backend, Codec, CodecDescriptor, CodecError, CodecKind, Converter};
use styx_format::FourCc;
use styx_frame::{CpuRead, FrameLease, Plane, PlanesMut};

use crate::kernel::{Path, convert_pixels, resample};
use crate::layout::{self, Kind, filter_for};

/// The name the software converter is registered and reported under.
pub const NAME: &str = "styx-scale-software";

/// A converter from `input` frames to `output` frames on the CPU. Scratch planes are kept across
/// calls, so a steady stream of one size allocates nothing after its first frame.
#[derive(Debug)]
pub struct Software {
    descriptor: CodecDescriptor,
    path: Path,
    luma: Vec<u8>,
    /// Chroma as interleaved U, V pairs, whatever the formats' order.
    chroma: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Software {
    /// A converter from `input` to `output`, on `path`. Fails at [`Converter::convert`] for a pair
    /// that has no exact conversion; the registry only offers those that do.
    pub fn new(input: FourCc, output: FourCc, path: Path) -> Self {
        Self {
            descriptor: describe(input, output),
            path,
            luma: Vec::new(),
            chroma: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        }
    }

    /// The path this converter runs.
    pub fn path(&self) -> Path {
        self.path
    }
}

impl<M> Codec<M> for Software {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

/// The descriptor of the software converter from `input` to `output`.
pub fn describe(input: FourCc, output: FourCc) -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Converter,
        input,
        output,
        backend: Backend::Native,
        name: NAME,
    }
}

fn plane<'a, M>(read: &CpuRead<'a, M>, index: usize) -> Result<Plane<'a>, CodecError> {
    read.plane(index)
        .ok_or_else(|| CodecError::Rejected(format!("the frame has no plane {index}")))
}

fn put_plane(
    out: &mut PlanesMut<'_>,
    index: usize,
    packed: &[u8],
    row_bytes: usize,
    rows: usize,
) -> Result<(), CodecError> {
    let mut plane = out
        .plane(index)
        .ok_or_else(|| CodecError::Rejected(format!("the output has no plane {index}")))?;
    for r in 0..rows {
        let dst = plane
            .row_bytes_mut(r, row_bytes)
            .ok_or_else(|| CodecError::Rejected(format!("output plane {index} is too small")))?;
        dst.copy_from_slice(&packed[r * row_bytes..][..row_bytes]);
    }
    Ok(())
}

fn size(buf: &mut Vec<u8>, len: usize) -> &mut [u8] {
    buf.resize(len, 0);
    &mut buf[..len]
}

impl<M> Converter<M> for Software {
    fn convert(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let (code_in, code_out) = (self.descriptor.input, self.descriptor.output);
        let (src, dst) = (input.format(), output.format());
        if src.code != code_in || dst.code != code_out {
            return Err(CodecError::Rejected(format!(
                "this converter takes {code_in} to {code_out}, got {} to {}",
                src.code, dst.code
            )));
        }
        let (sw, sh) = (
            src.resolution.width.get() as usize,
            src.resolution.height.get() as usize,
        );
        let (dw, dh) = (
            dst.resolution.width.get() as usize,
            dst.resolution.height.get() as usize,
        );
        if !layout::fits(code_in, sw, sh) || !layout::fits(code_out, dw, dh) {
            return Err(CodecError::Rejected(format!(
                "{sw}x{sh} to {dw}x{dh}: sides do not suit {code_in} or {code_out}"
            )));
        }
        let (Some(src_kind), Some(dst_kind)) = (layout::kind(code_in), layout::kind(code_out))
        else {
            return Err(CodecError::Rejected("format not handled".to_string()));
        };
        let filter = filter_for(sw, sh, dw, dh);
        let path = self.path;
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("the input frame is not readable".to_string()))?;
        let mut out = output
            .write()
            .ok_or_else(|| CodecError::Failed("the output frame is not writable".to_string()))?;

        match (src_kind, dst_kind) {
            (Kind::Gray, Kind::Gray) => {
                let p = plane(&read, 0)?;
                let luma = size(&mut self.luma, dw * dh);
                resample(path, filter, p.data, p.stride, (sw, sh), 1, luma, (dw, dh));
                put_plane(&mut out, 0, &self.luma, dw, dh)?;
            }
            (Kind::Rgb(from), Kind::Rgb(to)) => {
                let p = plane(&read, 0)?;
                let ch = from.bpp();
                let scaled = size(&mut self.luma, dw * dh * ch);
                resample(
                    path,
                    filter,
                    p.data,
                    p.stride,
                    (sw, sh),
                    ch,
                    scaled,
                    (dw, dh),
                );
                let mut plane_out = out
                    .plane(0)
                    .ok_or_else(|| CodecError::Rejected("the output has no plane 0".to_string()))?;
                for r in 0..dh {
                    let row = plane_out.row_bytes_mut(r, dw * to.bpp()).ok_or_else(|| {
                        CodecError::Rejected("output plane 0 is too small".to_string())
                    })?;
                    convert_pixels(path, from, to, &self.luma[r * dw * ch..], row, dw);
                }
            }
            (
                from @ (Kind::SemiPlanar { .. } | Kind::Planar),
                to @ (Kind::SemiPlanar { .. } | Kind::Planar),
            ) => {
                let (cw, ch_) = (dw / 2, dh / 2);
                let luma = plane(&read, 0)?;
                let scaled_luma = size(&mut self.luma, dw * dh);
                resample(
                    path,
                    filter,
                    luma.data,
                    luma.stride,
                    (sw, sh),
                    1,
                    scaled_luma,
                    (dw, dh),
                );
                // Chroma as interleaved U, V pairs, scaled at the chroma's own size.
                match from {
                    Kind::SemiPlanar { vu } => {
                        let c = plane(&read, 1)?;
                        let pairs = size(&mut self.chroma, cw * ch_ * 2);
                        resample(
                            path,
                            filter,
                            c.data,
                            c.stride,
                            (sw / 2, sh / 2),
                            2,
                            pairs,
                            (cw, ch_),
                        );
                        if vu {
                            for pair in pairs.as_chunks_mut::<2>().0 {
                                pair.swap(0, 1);
                            }
                        }
                    }
                    _ => {
                        let (u_src, v_src) = (plane(&read, 1)?, plane(&read, 2)?);
                        let u = size(&mut self.u, cw * ch_);
                        resample(
                            path,
                            filter,
                            u_src.data,
                            u_src.stride,
                            (sw / 2, sh / 2),
                            1,
                            u,
                            (cw, ch_),
                        );
                        let v = size(&mut self.v, cw * ch_);
                        resample(
                            path,
                            filter,
                            v_src.data,
                            v_src.stride,
                            (sw / 2, sh / 2),
                            1,
                            v,
                            (cw, ch_),
                        );
                        let pairs = size(&mut self.chroma, cw * ch_ * 2);
                        for (i, pair) in pairs.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                            pair[0] = self.u[i];
                            pair[1] = self.v[i];
                        }
                    }
                }
                put_plane(&mut out, 0, &self.luma, dw, dh)?;
                match to {
                    Kind::SemiPlanar { vu } => {
                        let mut plane_out = out.plane(1).ok_or_else(|| {
                            CodecError::Rejected("the output has no plane 1".to_string())
                        })?;
                        for r in 0..ch_ {
                            let row = plane_out.row_bytes_mut(r, dw).ok_or_else(|| {
                                CodecError::Rejected("output plane 1 is too small".to_string())
                            })?;
                            let pairs = &self.chroma[r * dw..][..dw];
                            let dst_pairs = row.as_chunks_mut::<2>().0;
                            for (dst_pair, src_pair) in
                                dst_pairs.iter_mut().zip(pairs.as_chunks::<2>().0)
                            {
                                if vu {
                                    dst_pair[0] = src_pair[1];
                                    dst_pair[1] = src_pair[0];
                                } else {
                                    *dst_pair = *src_pair;
                                }
                            }
                        }
                    }
                    _ => {
                        for (index, offset) in [(1usize, 0usize), (2, 1)] {
                            let mut plane_out = out.plane(index).ok_or_else(|| {
                                CodecError::Rejected(format!("the output has no plane {index}"))
                            })?;
                            for r in 0..ch_ {
                                let row = plane_out.row_bytes_mut(r, cw).ok_or_else(|| {
                                    CodecError::Rejected(format!(
                                        "output plane {index} is too small"
                                    ))
                                })?;
                                let pairs = &self.chroma[r * dw..][..dw];
                                for (i, sample) in row.iter_mut().enumerate() {
                                    *sample = pairs[2 * i + offset];
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                return Err(CodecError::Rejected(format!(
                    "{code_in} to {code_out} needs a colour conversion, which this converter does not do"
                )));
            }
        }
        Ok(())
    }
}

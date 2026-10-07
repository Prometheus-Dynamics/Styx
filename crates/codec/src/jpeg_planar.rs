//! JPEG from planar 4:2:0 YUV or grey pixels, for small, frequent images (previews,
//! `styx::preview`): the encoder state and output buffer are kept between images, and YUV is
//! encoded as it is (no colour conversion) where the encoder takes YUV planes.
//!
//! | backend | feature | YUV 4:2:0 input | C library |
//! |---|---|---|---|
//! | [`JpegBackend::Turbojpeg`] | `codec-turbojpeg` | planes as they are (`tj3CompressFromYUVPlanes8`) | libjpeg-turbo |
//! | [`JpegBackend::Mozjpeg`] | `codec-mozjpeg` | raw data (`jpeg_write_raw_data`), fastest settings | mozjpeg |
//! | [`JpegBackend::Image`] | `dynamic-image` | converted to RGB first (the `image` crate takes RGB or grey) | none (pure Rust) |
//!
//! `zune-jpeg` (feature `codec-zune`) only decodes. [`JpegBackend::Auto`] takes the first
//! available in the order above. mozjpeg and libjpeg-turbo export the same libjpeg symbols, so
//! one binary links one of them: with both features, mozjpeg is left out (as
//! `jpeg_encoder::MozjpegEncoder` is).

use crate::CodecError;

/// Pixels to encode.
#[derive(Clone, Copy, Debug)]
pub enum JpegInput<'a> {
    /// YUV 4:2:0 (JFIF: full-range BT.601): `y` is `width` x `height` with rows `y_stride`
    /// apart; `u` and `v` are `width.div_ceil(2)` x `height.div_ceil(2)` with rows `c_stride`
    /// apart.
    I420 {
        y: &'a [u8],
        u: &'a [u8],
        v: &'a [u8],
        width: usize,
        height: usize,
        y_stride: usize,
        c_stride: usize,
    },
    /// 8-bit grey, rows `stride` apart.
    Gray {
        y: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
    },
}

impl JpegInput<'_> {
    /// Width and height in pixels.
    pub fn size(&self) -> (usize, usize) {
        match *self {
            Self::I420 { width, height, .. } | Self::Gray { width, height, .. } => (width, height),
        }
    }

    /// Checks that the planes hold the rows the strides and sizes say.
    fn check(&self) -> Result<(), CodecError> {
        let fits = |plane: &[u8], stride: usize, w: usize, h: usize| {
            w > 0 && h > 0 && stride >= w && plane.len() >= stride * (h - 1) + w
        };
        let ok = match *self {
            Self::I420 {
                y,
                u,
                v,
                width,
                height,
                y_stride,
                c_stride,
            } => {
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                fits(y, y_stride, width, height)
                    && fits(u, c_stride, cw, ch)
                    && fits(v, c_stride, cw, ch)
            }
            Self::Gray {
                y,
                width,
                height,
                stride,
            } => fits(y, stride, width, height),
        };
        if ok {
            Ok(())
        } else {
            Err(CodecError::Codec(
                "jpeg input planes are smaller than their size".into(),
            ))
        }
    }
}

/// A JPEG encoder implementation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum JpegBackend {
    /// The first available: turbojpeg, mozjpeg, image.
    #[default]
    Auto,
    /// libjpeg-turbo (feature `codec-turbojpeg`): YUV planes as they are.
    Turbojpeg,
    /// mozjpeg (feature `codec-mozjpeg`, without `codec-turbojpeg`) with libjpeg-turbo's
    /// fastest settings: YUV as raw data.
    Mozjpeg,
    /// The `image` crate's pure-Rust encoder (feature `dynamic-image`): YUV converted to RGB.
    Image,
}

impl JpegBackend {
    /// The backends compiled in, fastest first.
    pub fn available() -> Vec<JpegBackend> {
        let mut out = Vec::new();
        if cfg!(feature = "codec-turbojpeg") {
            out.push(Self::Turbojpeg);
        }
        if cfg!(all(
            feature = "codec-mozjpeg",
            not(feature = "codec-turbojpeg")
        )) {
            out.push(Self::Mozjpeg);
        }
        if cfg!(feature = "dynamic-image") {
            out.push(Self::Image);
        }
        out
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Turbojpeg => "turbojpeg",
            Self::Mozjpeg => "mozjpeg",
            Self::Image => "image",
        }
    }
}

impl std::str::FromStr for JpegBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "turbojpeg" => Ok(Self::Turbojpeg),
            "mozjpeg" => Ok(Self::Mozjpeg),
            "image" => Ok(Self::Image),
            other => Err(format!(
                "unknown JPEG encoder {other:?} (auto, turbojpeg, mozjpeg, image)"
            )),
        }
    }
}

enum State {
    /// Without any backend compiled in (never made: `new` refuses).
    #[allow(dead_code)]
    Unavailable,
    #[cfg(feature = "codec-turbojpeg")]
    Turbo(Box<turbo::Turbo>),
    #[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
    Moz(moz::Moz),
    #[cfg(feature = "dynamic-image")]
    Image(Vec<u8>),
}

/// Encodes [`JpegInput`]s to JPEG, keeping the encoder and its buffers between images.
pub struct PlanarJpegEncoder {
    backend: JpegBackend,
    quality: u8,
    state: State,
}

impl std::fmt::Debug for PlanarJpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanarJpegEncoder")
            .field("backend", &self.backend)
            .field("quality", &self.quality)
            .finish()
    }
}

impl PlanarJpegEncoder {
    /// An encoder at `quality` (1–100). Fails when `backend` is not compiled in (or, for
    /// [`JpegBackend::Auto`], none is).
    pub fn new(backend: JpegBackend, quality: u8) -> Result<Self, CodecError> {
        let quality = quality.clamp(1, 100);
        let backend = match backend {
            JpegBackend::Auto => *JpegBackend::available().first().ok_or_else(|| {
                CodecError::Codec(
                    "no JPEG encoder compiled in: enable codec-turbojpeg, codec-mozjpeg or \
                     image"
                        .into(),
                )
            })?,
            other => other,
        };
        let state: Result<State, CodecError> = match backend {
            #[cfg(feature = "codec-turbojpeg")]
            JpegBackend::Turbojpeg => {
                turbo::Turbo::new(quality).map(|turbo| State::Turbo(Box::new(turbo)))
            }
            #[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
            JpegBackend::Mozjpeg => Ok(State::Moz(moz::Moz::default())),
            #[cfg(feature = "dynamic-image")]
            JpegBackend::Image => Ok(State::Image(Vec::new())),
            other => Err(CodecError::Codec(format!(
                "JPEG encoder {} is not compiled in (available: {:?})",
                other.name(),
                JpegBackend::available()
            ))),
        };
        Ok(Self {
            backend,
            quality,
            state: state?,
        })
    }

    /// The backend encoding (never [`JpegBackend::Auto`]).
    pub fn backend(&self) -> JpegBackend {
        self.backend
    }

    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Encode `input` into `out` (cleared first; its capacity reused).
    pub fn encode(&mut self, input: &JpegInput<'_>, out: &mut Vec<u8>) -> Result<(), CodecError> {
        input.check()?;
        out.clear();
        #[allow(unused_variables)]
        let quality = self.quality;
        match &mut self.state {
            State::Unavailable => Err(CodecError::Codec("no JPEG encoder compiled in".into())),
            #[cfg(feature = "codec-turbojpeg")]
            State::Turbo(turbo) => turbo.encode(input, out),
            #[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
            State::Moz(moz) => moz.encode(input, quality, out),
            #[cfg(feature = "dynamic-image")]
            State::Image(rgb) => pure::encode(input, quality, rgb, out),
        }
    }
}

#[cfg(feature = "codec-turbojpeg")]
mod turbo {
    use turbojpeg::{Compressor, Image, PixelFormat, Subsamp, YuvPlanesImage};

    use super::JpegInput;
    use crate::CodecError;

    pub(super) struct Turbo {
        compressor: Compressor,
        /// Room for the largest JPEG of the last size (`tj3JPEGBufSize`).
        scratch: Vec<u8>,
        /// Planes padded to whole blocks, for odd sizes.
        pad: [Vec<u8>; 3],
    }

    /// `plane` (`rows` rows `stride` apart) when it holds `need` bytes, else a copy in `pad`
    /// with its last row repeated.
    fn pad_rows<'a>(
        plane: &'a [u8],
        stride: usize,
        rows: usize,
        need: usize,
        pad: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        if plane.len() >= need {
            return plane;
        }
        let last = (rows - 1) * stride;
        pad.clear();
        pad.extend_from_slice(plane);
        pad.resize(rows * stride, 0);
        while pad.len() < need {
            pad.extend_from_within(last..last + stride);
        }
        pad
    }

    fn err(e: turbojpeg::Error) -> CodecError {
        CodecError::Codec(format!("turbojpeg: {e}"))
    }

    impl Turbo {
        pub(super) fn new(quality: u8) -> Result<Self, CodecError> {
            let mut compressor = Compressor::new().map_err(err)?;
            compressor.set_quality(i32::from(quality)).map_err(err)?;
            compressor.set_optimize(false).map_err(err)?;
            Ok(Self {
                compressor,
                scratch: Vec::new(),
                pad: Default::default(),
            })
        }

        pub(super) fn encode(
            &mut self,
            input: &JpegInput<'_>,
            out: &mut Vec<u8>,
        ) -> Result<(), CodecError> {
            let (width, height) = input.size();
            let subsamp = match input {
                JpegInput::I420 { .. } => Subsamp::Sub2x2,
                JpegInput::Gray { .. } => Subsamp::Gray,
            };
            let room = turbojpeg::compressed_buf_len(width, height, subsamp).map_err(err)?;
            if self.scratch.len() < room {
                self.scratch.resize(room, 0);
            }
            let len = match *input {
                JpegInput::I420 {
                    y,
                    u,
                    v,
                    y_stride,
                    c_stride,
                    ..
                } => {
                    // libjpeg-turbo reads planes padded to whole 4:2:0 blocks (an odd height's
                    // last row twice): pad short planes with their last row.
                    // (`tj3YUVPlaneSize`: stride x (padded rows - 1) + padded width.)
                    let (pw, ph) = (width.next_multiple_of(2), height.next_multiple_of(2));
                    let need_y = y_stride * (ph - 1) + pw;
                    let need_c = c_stride * (ph / 2 - 1) + pw / 2;
                    let [py, pu, pv] = &mut self.pad;
                    let y = pad_rows(y, y_stride, height, need_y, py);
                    let u = pad_rows(u, c_stride, height.div_ceil(2), need_c, pu);
                    let v = pad_rows(v, c_stride, height.div_ceil(2), need_c, pv);
                    self.compressor.compress_yuv_planes_to_slice(
                        &YuvPlanesImage {
                            y_plane: y,
                            u_plane: u,
                            v_plane: v,
                            width,
                            height,
                            y_stride,
                            u_stride: c_stride,
                            v_stride: c_stride,
                            subsamp,
                        },
                        &mut self.scratch,
                    )
                }
                JpegInput::Gray { y, stride, .. } => {
                    self.compressor.set_subsamp(subsamp).map_err(err)?;
                    self.compressor.compress_to_slice(
                        Image {
                            pixels: &y[..stride * (height - 1) + width],
                            width,
                            pitch: stride,
                            height,
                            format: PixelFormat::GRAY,
                        },
                        &mut self.scratch,
                    )
                }
            }
            .map_err(err)?;
            out.extend_from_slice(&self.scratch[..len]);
            Ok(())
        }
    }
}

#[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
mod moz {
    use mozjpeg::{ColorSpace, Compress};

    use super::JpegInput;
    use crate::CodecError;

    /// Planes padded to whole blocks and MCUs, as raw data input needs them.
    #[derive(Default)]
    pub(super) struct Moz {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    /// `src` (`w` x `h`, rows `stride` apart) into `dst` as `pw` x `ph`, edges repeated.
    fn pad(
        src: &[u8],
        stride: usize,
        (w, h): (usize, usize),
        (pw, ph): (usize, usize),
        dst: &mut Vec<u8>,
    ) {
        dst.resize(pw * ph, 0);
        for row in 0..ph {
            let line = &src[row.min(h - 1) * stride..][..w];
            let out = &mut dst[row * pw..][..pw];
            out[..w].copy_from_slice(line);
            let last = line[w - 1];
            out[w..].fill(last);
        }
    }

    impl Moz {
        pub(super) fn encode(
            &mut self,
            input: &JpegInput<'_>,
            quality: u8,
            out: &mut Vec<u8>,
        ) -> Result<(), CodecError> {
            let (width, height) = input.size();
            // Rows as mozjpeg hands them to libjpeg: whole 8x8 blocks of each component
            // (`row_stride`), and whole 16-row MCUs.
            let rows = height.next_multiple_of(16);
            let gray = matches!(input, JpegInput::Gray { .. });
            match *input {
                JpegInput::I420 {
                    y,
                    u,
                    v,
                    y_stride,
                    c_stride,
                    ..
                } => {
                    let c = (width.div_ceil(2), height.div_ceil(2));
                    let cs = (width.div_ceil(16) * 8, rows / 2);
                    pad(
                        y,
                        y_stride,
                        (width, height),
                        (width.next_multiple_of(8), rows),
                        &mut self.y,
                    );
                    pad(u, c_stride, c, cs, &mut self.u);
                    pad(v, c_stride, c, cs, &mut self.v);
                }
                JpegInput::Gray { y, stride, .. } => {
                    pad(
                        y,
                        stride,
                        (width, height),
                        (width.next_multiple_of(8), rows),
                        &mut self.y,
                    );
                }
            }
            // libjpeg errors unwind through mozjpeg's handler.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let space = if gray {
                    ColorSpace::JCS_GRAYSCALE
                } else {
                    ColorSpace::JCS_YCbCr
                };
                let mut comp = Compress::new(space);
                comp.set_fastest_defaults();
                comp.set_size(width, height);
                comp.set_quality(f32::from(quality));
                comp.set_raw_data_in(true);
                if !gray {
                    comp.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
                }
                let mut started = comp.start_compress(std::mem::take(out))?;
                let planes: Vec<&[u8]> = if gray {
                    vec![&self.y]
                } else {
                    vec![&self.y, &self.u, &self.v]
                };
                if !started.write_raw_data(&planes) {
                    return Err(std::io::Error::other("raw data not written"));
                }
                started.finish()
            }));
            match result {
                Ok(Ok(jpeg)) => {
                    *out = jpeg;
                    Ok(())
                }
                Ok(Err(e)) => Err(CodecError::Codec(format!("mozjpeg: {e}"))),
                Err(_) => Err(CodecError::Codec("mozjpeg: encoder error".into())),
            }
        }
    }
}

#[cfg(feature = "dynamic-image")]
mod pure {
    use image::ImageEncoder;
    use image::codecs::jpeg::JpegEncoder;

    use super::JpegInput;
    use crate::CodecError;

    /// Full-range BT.601 (JFIF) YUV 4:2:0 to RGB24, in fixed point.
    fn i420_to_rgb(input: &JpegInput<'_>, rgb: &mut Vec<u8>) {
        let JpegInput::I420 {
            y,
            u,
            v,
            width,
            height,
            y_stride,
            c_stride,
        } = *input
        else {
            return;
        };
        rgb.resize(width * height * 3, 0);
        for row in 0..height {
            let ys = &y[row * y_stride..][..width];
            let us = &u[(row / 2) * c_stride..];
            let vs = &v[(row / 2) * c_stride..];
            let out = &mut rgb[row * width * 3..][..width * 3];
            for (col, px) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let luma = i32::from(ys[col]) << 16;
                let cb = i32::from(us[col / 2]) - 128;
                let cr = i32::from(vs[col / 2]) - 128;
                let clamp = |x: i32| ((x + 32768) >> 16).clamp(0, 255) as u8;
                px[0] = clamp(luma + 91881 * cr);
                px[1] = clamp(luma - 22554 * cb - 46802 * cr);
                px[2] = clamp(luma + 116130 * cb);
            }
        }
    }

    pub(super) fn encode(
        input: &JpegInput<'_>,
        quality: u8,
        rgb: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        let (width, height) = input.size();
        let (pixels, color) = match *input {
            JpegInput::I420 { .. } => {
                i420_to_rgb(input, rgb);
                (&rgb[..], image::ExtendedColorType::Rgb8)
            }
            JpegInput::Gray { y, stride, .. } => {
                if stride == width {
                    (&y[..width * height], image::ExtendedColorType::L8)
                } else {
                    rgb.clear();
                    for row in 0..height {
                        rgb.extend_from_slice(&y[row * stride..][..width]);
                    }
                    (&rgb[..], image::ExtendedColorType::L8)
                }
            }
        };
        JpegEncoder::new_with_quality(&mut *out, quality)
            .write_image(pixels, width as u32, height as u32, color)
            .map_err(|e| CodecError::Codec(format!("image jpeg: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let y = (0..width * height).map(|i| (i % 251) as u8).collect();
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        (y, vec![100; cw * ch], vec![160; cw * ch])
    }

    #[test]
    fn every_backend_encodes_odd_sizes_and_grey() {
        for backend in JpegBackend::available() {
            let mut encoder = PlanarJpegEncoder::new(backend, 70).unwrap();
            assert_eq!(encoder.backend(), backend);
            for (w, h) in [(64, 40), (37, 21), (320, 200)] {
                let (y, u, v) = gradient(w, h);
                let mut out = Vec::new();
                let input = JpegInput::I420 {
                    y: &y,
                    u: &u,
                    v: &v,
                    width: w,
                    height: h,
                    y_stride: w,
                    c_stride: w.div_ceil(2),
                };
                encoder.encode(&input, &mut out).unwrap();
                assert_eq!(&out[..2], &[0xFF, 0xD8], "{backend:?} {w}x{h}");
                assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9]);
                let gray = JpegInput::Gray {
                    y: &y,
                    width: w,
                    height: h,
                    stride: w,
                };
                encoder.encode(&gray, &mut out).unwrap();
                assert_eq!(&out[..2], &[0xFF, 0xD8]);
            }
        }
    }

    #[test]
    fn short_planes_are_refused() {
        let Some(&backend) = JpegBackend::available().first() else {
            return;
        };
        let mut encoder = PlanarJpegEncoder::new(backend, 70).unwrap();
        let y = vec![0u8; 10];
        let input = JpegInput::Gray {
            y: &y,
            width: 8,
            height: 8,
            stride: 8,
        };
        assert!(encoder.encode(&input, &mut Vec::new()).is_err());
        assert!(PlanarJpegEncoder::new(JpegBackend::Auto, 70).is_ok());
    }
}

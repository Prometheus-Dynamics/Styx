//! A frame to a small planar YUV 4:2:0 (or grey) picture for the JPEG encoder.
//!
//! - **Source:** the frame, or its smallest companion that still covers the preview size (a
//!   pyramid level, the ISP's scaled output, the overview of a region), colour preferred.
//! - **Scaling:** 2x2 box halvings while the source is at least twice the target (SIMD for
//!   packed planes: `styx_core::simd::box2_row`), then one bilinear pass to the exact size.
//!   Chroma is scaled from its own planes (NV12's interleaved UV, YUYV's samples, I420's
//!   planes) without converting the frame first.
//! - **Inputs:** NV12, NV21, I420 (YU12), YV12, YUYV, UYVY, grey (GREY, R8) and RGB24
//!   (RG24/BG24, scaled per channel, converted at the small size). Other formats are refused.
//!
//! Buffers are kept between frames: nothing is allocated per frame once they have grown.

use styx_core::prelude::*;

/// The picture the encoder takes: `y` is `width` x `height`, `u` and `v` `width / 2` x
/// `height / 2` (both even), rows packed. Grey pictures have no chroma.
#[derive(Default)]
pub(super) struct Picture {
    pub(super) y: Vec<u8>,
    pub(super) u: Vec<u8>,
    pub(super) v: Vec<u8>,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) gray: bool,
}

/// Samples of one plane: `width` x `height`, rows `stride` bytes apart, each sample `step`
/// bytes after the previous, the first `offset` bytes into its row.
#[derive(Clone, Copy)]
struct View<'a> {
    data: &'a [u8],
    stride: usize,
    width: usize,
    height: usize,
    step: usize,
    offset: usize,
}

impl<'a> View<'a> {
    fn packed(data: &'a [u8], width: usize, height: usize) -> Self {
        Self {
            data,
            stride: width,
            width,
            height,
            step: 1,
            offset: 0,
        }
    }

    fn row(&self, y: usize) -> &'a [u8] {
        &self.data[y * self.stride + self.offset..]
    }

    fn fits(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.data.len()
                > (self.height - 1) * self.stride + self.offset + (self.width - 1) * self.step
    }
}

/// Scales frames into a [`Picture`], keeping its buffers.
#[derive(Default)]
pub(super) struct Scaler {
    pub(super) picture: Picture,
    /// Halving ping-pong buffers.
    a: Vec<u8>,
    b: Vec<u8>,
    /// RGB sources: the scaled red, green and blue planes, and the first halving's.
    rgb: [Vec<u8>; 3],
    rgb_half: [Vec<u8>; 3],
    /// Bilinear taps (source index, weight) along each axis.
    xs: Vec<(u32, u32)>,
    ys: Vec<(u32, u32)>,
}

/// The size a `width` x `height` frame is shown at within `max`: the aspect ratio kept, never
/// larger than the frame, both even (at least 2x2).
pub(super) fn fit((width, height): (u32, u32), (max_w, max_h): (u32, u32)) -> (usize, usize) {
    let scale = (f64::from(max_w) / f64::from(width))
        .min(f64::from(max_h) / f64::from(height))
        .min(1.0);
    let even = |v: f64| ((v.floor() as usize) & !1).max(2);
    (
        even(f64::from(width) * scale),
        even(f64::from(height) * scale),
    )
}

/// The frame (or companion) to scale: the smallest at least `target`'s size, else the largest;
/// colour before grey when the frame is in colour; the overview before a region of interest.
pub(super) fn source(frame: &FrameLease, target: (u32, u32)) -> &FrameLease {
    let colour = |f: &FrameLease| !matches!(kind_of(f.meta().format.code), Some(Kind::Gray) | None);
    let want_colour = colour(frame);
    let mut candidates: Vec<&FrameLease> = vec![frame];
    for (kind, companion) in frame.companions() {
        let usable = match kind {
            CompanionKind::Pyramid { .. } | CompanionKind::Scaled => frame.meta().crop.is_none(),
            CompanionKind::Overview => true,
            CompanionKind::Region { .. } => false,
        };
        if usable && kind_of(companion.meta().format.code).is_some() {
            candidates.push(companion);
        }
    }
    let key = |f: &&FrameLease| {
        let res = f.meta().format.resolution;
        let (w, h) = (res.width.get(), res.height.get());
        let area = u64::from(w) * u64::from(h);
        let covers = w >= target.0 && h >= target.1;
        (
            want_colour && !colour(f),
            // A region view (cropped) when the overview shows the whole frame.
            f.meta().crop.is_some(),
            !covers,
            if covers { area } else { u64::MAX - area },
        )
    };
    candidates.into_iter().min_by_key(key).unwrap_or(frame)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Nv12,
    Nv21,
    I420,
    Yv12,
    Yuyv,
    Uyvy,
    Gray,
    Rgb,
    Bgr,
}

fn kind_of(code: FourCc) -> Option<Kind> {
    Some(match &code.to_u32().to_le_bytes() {
        b"NV12" => Kind::Nv12,
        b"NV21" => Kind::Nv21,
        b"YU12" | b"I420" => Kind::I420,
        b"YV12" => Kind::Yv12,
        b"YUYV" | b"YUY2" => Kind::Yuyv,
        b"UYVY" => Kind::Uyvy,
        b"GREY" | b"R8  " | b"Y800" => Kind::Gray,
        b"RG24" | b"RGB3" => Kind::Rgb,
        b"BG24" | b"BGR3" => Kind::Bgr,
        _ => return None,
    })
}

impl Scaler {
    /// Scale `frame` into [`Scaler::picture`] at `size` (from [`fit`]).
    pub(super) fn scale(
        &mut self,
        frame: &FrameLease,
        (dw, dh): (usize, usize),
    ) -> Result<(), String> {
        let meta = frame.meta();
        let kind = kind_of(meta.format.code)
            .ok_or_else(|| format!("{} frames are not previewed", meta.format.code))?;
        let (w, h) = (
            meta.format.resolution.width.get() as usize,
            meta.format.resolution.height.get() as usize,
        );
        let planes = frame.planes();
        let data = |i: usize| planes.get(i).map(|p| (p.data(), p.stride()));
        let (y, ys) = data(0).ok_or("frame without planes")?;
        if y.is_empty() {
            return Err("frame memory is not readable here".into());
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let plane = |data, stride, width, height, step, offset| View {
            data,
            stride,
            width,
            height,
            step,
            offset,
        };
        // The chroma plane(s) of planar formats: their own planes, or after the luma rows.
        let after = |i: usize, rows: usize, stride: usize| -> (&[u8], usize) {
            data(i).unwrap_or((y.get(rows * ys..).unwrap_or_default(), stride))
        };
        self.picture.width = dw;
        self.picture.height = dh;
        self.picture.gray = kind == Kind::Gray;
        let (cdw, cdh) = (dw / 2, dh / 2);
        let luma = match kind {
            Kind::Yuyv => plane(y, ys, w, h, 2, 0),
            Kind::Uyvy => plane(y, ys, w, h, 2, 1),
            Kind::Rgb | Kind::Bgr => return self.scale_rgb(y, ys, (w, h), kind, (dw, dh)),
            _ => plane(y, ys, w, h, 1, 0),
        };
        let chroma = match kind {
            Kind::Nv12 | Kind::Nv21 => {
                let (uv, uvs) = after(1, h, ys);
                let (u, v) = if kind == Kind::Nv12 { (0, 1) } else { (1, 0) };
                Some((plane(uv, uvs, cw, ch, 2, u), plane(uv, uvs, cw, ch, 2, v)))
            }
            Kind::I420 | Kind::Yv12 => {
                let cs = planes.get(1).map_or(ys / 2, |p| p.stride());
                let (first, _) = after(1, h, ys);
                let second = match data(2) {
                    Some((d, _)) => d,
                    None => first.get(ch * cs..).unwrap_or_default(),
                };
                let (u, v) = if kind == Kind::I420 {
                    (first, second)
                } else {
                    (second, first)
                };
                Some((plane(u, cs, cw, ch, 1, 0), plane(v, cs, cw, ch, 1, 0)))
            }
            Kind::Yuyv => Some((plane(y, ys, cw, h, 4, 1), plane(y, ys, cw, h, 4, 3))),
            Kind::Uyvy => Some((plane(y, ys, cw, h, 4, 0), plane(y, ys, cw, h, 4, 2))),
            Kind::Gray | Kind::Rgb | Kind::Bgr => None,
        };
        let mut out = std::mem::take(&mut self.picture.y);
        let done = self.scale_plane(luma, (dw, dh), &mut out);
        self.picture.y = out;
        done?;
        if let Some((u, v)) = chroma {
            for (src, which) in [(u, 0), (v, 1)] {
                let mut out = std::mem::take(if which == 0 {
                    &mut self.picture.u
                } else {
                    &mut self.picture.v
                });
                let done = self.scale_plane(src, (cdw, cdh), &mut out);
                *(if which == 0 {
                    &mut self.picture.u
                } else {
                    &mut self.picture.v
                }) = out;
                done?;
            }
        }
        Ok(())
    }

    /// `src` scaled to `dw` x `dh` into `out` (packed rows).
    fn scale_plane(
        &mut self,
        src: View<'_>,
        (dw, dh): (usize, usize),
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if !src.fits() {
            return Err("frame plane is smaller than its format says".into());
        }
        let (mut a, mut b) = (std::mem::take(&mut self.a), std::mem::take(&mut self.b));
        let halving = |w: usize, h: usize| w >= 2 * dw && h >= 2 * dh;
        // The first halving reads the frame; the next ones `a`, into `b`, swapped back.
        let mut size = None;
        if halving(src.width, src.height) {
            let (mut w, mut h) = halve(src, &mut a);
            while halving(w, h) {
                (w, h) = halve(View::packed(&a, w, h), &mut b);
                std::mem::swap(&mut a, &mut b);
            }
            size = Some((w, h));
        }
        let current = match size {
            Some((w, h)) => View::packed(&a, w, h),
            None => src,
        };
        out.resize(dw * dh, 0);
        if (current.width, current.height) == (dw, dh) {
            for (row, line) in out.chunks_exact_mut(dw).enumerate() {
                copy_row(current, row, line);
            }
        } else {
            taps(current.width, dw, &mut self.xs);
            taps(current.height, dh, &mut self.ys);
            bilinear(current, &self.xs, &self.ys, out, dw);
        }
        self.a = a;
        self.b = b;
        Ok(())
    }

    /// RGB24 frames: each channel scaled, then converted to YUV 4:2:0 at the small size.
    fn scale_rgb(
        &mut self,
        data: &[u8],
        stride: usize,
        (w, h): (usize, usize),
        kind: Kind,
        (dw, dh): (usize, usize),
    ) -> Result<(), String> {
        let mut rgb = std::mem::take(&mut self.rgb);
        // The first halving reads the frame once for all three channels.
        let mut half = std::mem::take(&mut self.rgb_half);
        let halved = w >= 2 * dw && h >= 2 * dh;
        if halved {
            halve_rgb(data, stride, (w, h), kind == Kind::Bgr, &mut half);
        }
        for (channel, plane) in rgb.iter_mut().enumerate() {
            let view = if halved {
                View::packed(&half[channel], w / 2, h / 2)
            } else {
                View {
                    data,
                    stride,
                    width: w,
                    height: h,
                    step: 3,
                    offset: if kind == Kind::Bgr {
                        2 - channel
                    } else {
                        channel
                    },
                }
            };
            if let Err(err) = self.scale_plane(view, (dw, dh), plane) {
                self.rgb = rgb;
                self.rgb_half = half;
                return Err(err);
            }
        }
        self.rgb_half = half;
        rgb_to_i420(&rgb, (dw, dh), &mut self.picture);
        self.rgb = rgb;
        Ok(())
    }
}

/// RGB24 rows (`bgr`: blue first) halved with a rounded 2x2 box filter into red, green and blue
/// planes, reading each pixel once.
fn halve_rgb(
    data: &[u8],
    stride: usize,
    (w, h): (usize, usize),
    bgr: bool,
    out: &mut [Vec<u8>; 3],
) {
    let (ow, oh) = (w / 2, h / 2);
    for plane in out.iter_mut() {
        plane.resize(ow * oh, 0);
    }
    let [r, g, b] = out;
    let (first, third) = if bgr { (b, r) } else { (r, b) };
    for y in 0..oh {
        let top = &data[2 * y * stride..][..ow * 6];
        let bottom = &data[(2 * y + 1) * stride..][..ow * 6];
        let rows = top.as_chunks::<6>().0.iter().zip(bottom.as_chunks::<6>().0);
        let outs = first[y * ow..][..ow]
            .iter_mut()
            .zip(&mut g[y * ow..][..ow])
            .zip(&mut third[y * ow..][..ow]);
        for ((t, b), ((c0, c1), c2)) in rows.zip(outs) {
            let avg = |i: usize| {
                let sum =
                    u16::from(t[i]) + u16::from(t[i + 3]) + u16::from(b[i]) + u16::from(b[i + 3]);
                ((sum + 2) >> 2) as u8
            };
            (*c0, *c1, *c2) = (avg(0), avg(1), avg(2));
        }
    }
}

/// Row `row` of `src` into `out` (`src.width` samples).
fn copy_row(src: View<'_>, row: usize, out: &mut [u8]) {
    let line = src.row(row);
    if src.step == 1 {
        out.copy_from_slice(&line[..out.len()]);
    } else {
        for (x, o) in out.iter_mut().enumerate() {
            *o = line[x * src.step];
        }
    }
}

/// `src` halved with a rounded 2x2 box filter into `dst` (packed); returns the new size.
fn halve(src: View<'_>, dst: &mut Vec<u8>) -> (usize, usize) {
    let (w, h) = (src.width / 2, src.height / 2);
    dst.resize(w * h, 0);
    for (y, out) in dst.chunks_exact_mut(w).enumerate() {
        let (top, bottom) = (src.row(2 * y), src.row(2 * y + 1));
        if src.step == 1 {
            styx_core::simd::box2_row(top, bottom, out, w);
        } else {
            let s = src.step;
            for (x, o) in out.iter_mut().enumerate() {
                let (i, j) = (2 * x * s, (2 * x + 1) * s);
                let sum = u16::from(top[i]) + u16::from(top[j]);
                let sum = sum + u16::from(bottom[i]) + u16::from(bottom[j]);
                *o = ((sum + 2) >> 2) as u8;
            }
        }
    }
    (w, h)
}

/// Bilinear taps from `from` samples to `to`, pixel centres aligned: for each output sample
/// the first source sample and the second one's weight (16 bits).
fn taps(from: usize, to: usize, out: &mut Vec<(u32, u32)>) {
    out.clear();
    let scale = from as f64 / to as f64;
    for i in 0..to {
        let pos = ((i as f64 + 0.5) * scale - 0.5).clamp(0.0, (from - 1) as f64);
        let first = (pos.floor() as usize).min(from.saturating_sub(2));
        let weight = ((pos - first as f64) * 65536.0).round().clamp(0.0, 65536.0) as u32;
        out.push((first as u32, weight));
    }
}

fn bilinear(src: View<'_>, xs: &[(u32, u32)], ys: &[(u32, u32)], out: &mut [u8], dw: usize) {
    let s = src.step;
    let second = |first: usize, len: usize| (first + 1).min(len - 1);
    for (line, &(y0, wy)) in out.chunks_exact_mut(dw).zip(ys) {
        let y0 = y0 as usize;
        let (top, bottom) = (src.row(y0), src.row(second(y0, src.height)));
        for (o, &(x0, wx)) in line.iter_mut().zip(xs) {
            let x0 = x0 as usize;
            let x1 = second(x0, src.width);
            let (i, j) = (x0 * s, x1 * s);
            let lerp =
                |a: u8, b: u8| u64::from(a) * u64::from(65536 - wx) + u64::from(b) * u64::from(wx);
            let t = lerp(top[i], top[j]);
            let b = lerp(bottom[i], bottom[j]);
            let v = (t * u64::from(65536 - wy) + b * u64::from(wy) + (1 << 31)) >> 32;
            *o = v.min(255) as u8;
        }
    }
}

/// Full-range BT.601 (JFIF) RGB planes to YUV 4:2:0, chroma from each 2x2 block's mean.
fn rgb_to_i420(rgb: &[Vec<u8>; 3], (w, h): (usize, usize), picture: &mut Picture) {
    picture.gray = false;
    picture.y.resize(w * h, 0);
    picture.u.resize(w / 2 * (h / 2), 0);
    picture.v.resize(w / 2 * (h / 2), 0);
    let px = |i: usize| {
        (
            i32::from(rgb[0][i]),
            i32::from(rgb[1][i]),
            i32::from(rgb[2][i]),
        )
    };
    for i in 0..w * h {
        let (r, g, b) = px(i);
        picture.y[i] = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8;
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = px((2 * cy + dy) * w + 2 * cx + dx);
                (r, g, b) = (r + p.0, g + p.1, b + p.2);
            }
            let c = cy * (w / 2) + cx;
            let u = (-11059 * r - 21709 * g + 32768 * b + (128 << 18) + (1 << 17)) >> 18;
            let v = (32768 * r - 27439 * g - 5329 * b + (128 << 18) + (1 << 17)) >> 18;
            picture.u[c] = u.clamp(0, 255) as u8;
            picture.v[c] = v.clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(code: FourCc, w: u32, h: u32, bytes: Vec<u8>) -> FrameLease {
        let format = MediaFormat::new(code, Resolution::new(w, h).unwrap(), ColorSpace::Srgb);
        FrameLease::from_visible_bytes(format, 5, &bytes).unwrap()
    }

    fn nv12(w: usize, h: usize, y: impl Fn(usize, usize) -> u8, uv: (u8, u8)) -> Vec<u8> {
        let mut out: Vec<u8> = (0..w * h).map(|i| y(i % w, i / w)).collect();
        for _ in 0..w / 2 * (h / 2) {
            out.extend([uv.0, uv.1]);
        }
        out
    }

    #[test]
    fn fit_keeps_the_aspect_ratio_and_even_sizes() {
        assert_eq!(fit((1280, 800), (640, 400)), (640, 400));
        assert_eq!(fit((1920, 1080), (640, 400)), (640, 360));
        assert_eq!(fit((1280, 800), (320, 320)), (320, 200));
        assert_eq!(fit((320, 200), (640, 400)), (320, 200), "never upscaled");
        assert_eq!(fit((101, 51), (640, 400)), (100, 50));
    }

    #[test]
    fn nv12_halves_and_resamples_luma_and_chroma() {
        let bytes = nv12(1280, 800, |x, _| (x / 5) as u8, (90, 200));
        let src = frame(FourCc::NV12, 1280, 800, bytes);
        let mut scaler = Scaler::default();
        // 1280 -> 640 by one halving; 1280 -> 480 by one halving and a bilinear pass.
        for size in [(640, 400), (480, 300), (320, 200)] {
            scaler.scale(&src, size).unwrap();
            let p = &scaler.picture;
            assert_eq!((p.width, p.height, p.gray), (size.0, size.1, false));
            assert_eq!(p.y.len(), size.0 * size.1);
            assert_eq!(p.u.len(), size.0 / 2 * (size.1 / 2));
            assert!(p.u.iter().all(|&u| u == 90) && p.v.iter().all(|&v| v == 200));
            // The gradient survives: left dark, right bright, monotonic along a row.
            let row = &p.y[..size.0];
            assert!(row.windows(2).all(|w| w[0] <= w[1]), "{size:?}");
            assert!(
                row[0] < 5 && row[size.0 - 1] > 245,
                "{} {}",
                row[0],
                row[size.0 - 1]
            );
        }
    }

    #[test]
    fn packed_and_planar_formats_scale_alike() {
        // A flat colour in every format gives the same picture.
        let (w, h) = (64usize, 32usize);
        let yuyv: Vec<u8> = (0..w / 2 * h).flat_map(|_| [100u8, 60, 100, 220]).collect();
        let uyvy: Vec<u8> = (0..w / 2 * h).flat_map(|_| [60u8, 100, 220, 100]).collect();
        let mut i420 = vec![100u8; w * h];
        i420.extend(vec![60u8; w / 2 * (h / 2)]);
        i420.extend(vec![220u8; w / 2 * (h / 2)]);
        let mut yv12 = vec![100u8; w * h];
        yv12.extend(vec![220u8; w / 2 * (h / 2)]);
        yv12.extend(vec![60u8; w / 2 * (h / 2)]);
        let nv21 = nv12(w, h, |_, _| 100, (220, 60));
        for (code, bytes) in [
            (FourCc::YUYV, yuyv),
            (FourCc::new(*b"UYVY"), uyvy),
            (FourCc::new(*b"YU12"), i420),
            (FourCc::new(*b"YV12"), yv12),
            (FourCc::new(*b"NV21"), nv21),
        ] {
            let mut scaler = Scaler::default();
            scaler
                .scale(&frame(code, w as u32, h as u32, bytes), (16, 8))
                .unwrap();
            let p = &scaler.picture;
            assert!(p.y.iter().all(|&v| v == 100), "{code}");
            assert!(p.u.iter().all(|&v| v == 60), "{code} {:?}", &p.u[..4]);
            assert!(p.v.iter().all(|&v| v == 220), "{code}");
        }
    }

    #[test]
    fn grey_and_rgb_sources() {
        let mut scaler = Scaler::default();
        scaler
            .scale(&frame(FourCc::GREY, 64, 64, vec![77; 64 * 64]), (32, 32))
            .unwrap();
        assert!(scaler.picture.gray && scaler.picture.y.iter().all(|&v| v == 77));
        // Pure red, as RGB and BGR.
        let red: Vec<u8> = (0..64 * 32).flat_map(|_| [255u8, 0, 0]).collect();
        let blue_first: Vec<u8> = (0..64 * 32).flat_map(|_| [0u8, 0, 255]).collect();
        for (code, bytes) in [(FourCc::RG24, red), (FourCc::new(*b"BG24"), blue_first)] {
            scaler.scale(&frame(code, 64, 32, bytes), (16, 8)).unwrap();
            let p = &scaler.picture;
            assert!(!p.gray);
            assert!(p.y.iter().all(|&v| v == 76), "{code} {}", p.y[0]);
            assert!(p.v.iter().all(|&v| v == 255), "{code} {}", p.v[0]);
            assert!(
                p.u.iter().all(|&v| (84..=86).contains(&v)),
                "{code} {}",
                p.u[0]
            );
        }
        let format = MediaFormat::new(
            FourCc::MJPG,
            Resolution::new(8, 8).unwrap(),
            ColorSpace::Srgb,
        );
        let mut buf = BufferPool::with_limits(1, 16, 0).lease();
        buf.resize(16);
        let jpeg = FrameLease::single_plane(FrameMeta::new(format, 1), buf, 16, 16);
        assert!(scaler.scale(&jpeg, (8, 8)).is_err());
    }

    #[test]
    fn the_smallest_covering_colour_companion_is_scaled() {
        let full = frame(
            FourCc::NV12,
            1280,
            800,
            nv12(1280, 800, |_, _| 1, (128, 128)),
        );
        let half = frame(FourCc::NV12, 640, 400, nv12(640, 400, |_, _| 2, (128, 128)));
        let quarter_grey = frame(FourCc::GREY, 320, 200, vec![3; 320 * 200]);
        let full = full
            .with_companion(CompanionKind::Scaled, half)
            .unwrap()
            .with_companion(CompanionKind::Pyramid { level: 2 }, quarter_grey)
            .unwrap();
        let w = |f: &FrameLease| f.meta().format.resolution.width.get();
        assert_eq!(w(source(&full, (640, 400))), 640);
        assert_eq!(w(source(&full, (320, 200))), 640, "colour first");
        assert_eq!(w(source(&full, (1280, 800))), 1280);
        let grey = frame(FourCc::GREY, 1280, 800, vec![0; 1280 * 800])
            .with_box_pyramid(2, 1)
            .unwrap();
        assert_eq!(w(source(&grey, (320, 200))), 320);
        assert_eq!(w(source(&grey, (300, 150))), 320);
    }
}

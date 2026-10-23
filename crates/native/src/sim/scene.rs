//! What a simulated sensor sees, and how a frame of it is exposed and read out.
//!
//! A [`SimScene`] gives the light on the pixel array: per pixel, the fraction of full scale it
//! collects per second of exposure at gain 1, by colour. [`render`] exposes it as the sensor
//! does for a frame: the frame's exposure and gain (the values the control schedule applied to
//! it), the black level, a little shot and read noise, clipping, the flips (the array read out
//! mirrored, so the colour filter order of the frame changes with them, as on the device) and
//! the receiver's memory format.

use std::sync::{Arc, Mutex};

use styx_sensor::ColorFilter;

use crate::formats::RawPacking;

/// A frame of a scene: the light on each pixel of a `width`×`height` image.
#[derive(Clone, Debug)]
pub enum SceneFrame {
    /// Per pixel, red, green and blue (fraction of full scale per second at gain 1), shared
    /// between frames, with a bright `spot` over it (the moving part of the scene).
    Rgb {
        width: u32,
        height: u32,
        light: Arc<Vec<[f32; 3]>>,
        spot: Option<Spot>,
    },
    /// One colour per pixel in the colour filter order `order` of the unflipped array (a raw
    /// recording's frames, with their exposure and gain taken out).
    Mosaic {
        width: u32,
        height: u32,
        order: ColorFilter,
        light: Vec<f32>,
    },
}

/// A rectangle of even light over a scene frame (pixels `x0..x1`, `y0..y1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub light: [f32; 3],
}

/// What a simulated camera sees.
pub trait SimScene: Send + Sync {
    /// Frame `index` of the scene as a `width`×`height` image of the part of the pixel array
    /// a mode reads (`field`: left, top, right, bottom as fractions of the array).
    fn frame(&self, index: u64, width: u32, height: u32, field: [f32; 4]) -> Arc<SceneFrame>;
}

/// sRGB (0..1) to linear light.
fn linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The static chart rendered for a size and field (the field's bits).
type ChartCache = (u32, u32, [u32; 4], Arc<Vec<[f32; 3]>>);

/// A test chart: a grey ramp, the 24 colours of a colour checker, a fine checkerboard and a
/// bright square that moves across it (so frames differ and motion can be followed), lit so
/// that mid grey is about 18% of full scale at 10 ms and gain 1 times `light`.
pub struct ChartScene {
    light: f32,
    cache: Mutex<Option<ChartCache>>,
}

/// The X-Rite ColorChecker's patches in sRGB (0..255), row by row.
const CHECKER: [[u8; 3]; 24] = [
    [115, 82, 68],
    [194, 150, 130],
    [98, 122, 157],
    [87, 108, 67],
    [133, 128, 177],
    [103, 189, 170],
    [214, 126, 44],
    [80, 91, 166],
    [193, 90, 99],
    [94, 60, 108],
    [157, 188, 64],
    [224, 163, 46],
    [56, 61, 150],
    [70, 148, 73],
    [175, 54, 60],
    [231, 199, 31],
    [187, 86, 149],
    [8, 133, 161],
    [243, 243, 242],
    [200, 200, 200],
    [160, 160, 160],
    [122, 122, 121],
    [85, 85, 85],
    [52, 52, 52],
];

impl ChartScene {
    /// A chart lit `light` times the default (1: mid grey at 18% of full scale at 10 ms and gain
    /// 1).
    pub fn new(light: f32) -> Self {
        Self {
            light,
            cache: Mutex::new(None),
        }
    }

    /// The static chart, in linear reflectance, at `(u, v)` (fractions of the array).
    fn reflectance(u: f32, v: f32) -> [f32; 3] {
        // The colour checker in the middle: 6 x 4 patches over 60% x 45% of the field.
        let (cx0, cy0, cw, ch) = (0.2, 0.2, 0.6, 0.45);
        if (cx0..cx0 + cw).contains(&u) && (cy0..cy0 + ch).contains(&v) {
            let (fx, fy) = ((u - cx0) / cw * 6.0, (v - cy0) / ch * 4.0);
            let (col, row) = (fx as usize, fy as usize);
            // A dark gap between patches.
            if fx.fract() < 0.08 || fy.fract() < 0.08 {
                return [0.03; 3];
            }
            let c = CHECKER[(row.min(3)) * 6 + col.min(5)];
            return c.map(|x| linear(f32::from(x) / 255.0));
        }
        // A fine checkerboard band below it (detail, sharpness).
        if (0.72..0.82).contains(&v) && (0.2..0.8).contains(&u) {
            let on = ((u * 160.0) as i32 + (v * 100.0) as i32) % 2 == 0;
            return [if on { 0.8 } else { 0.05 }; 3];
        }
        // A grey ramp along the top, the rest a dim wall.
        if v < 0.12 {
            return [0.02 + 0.88 * u; 3];
        }
        [0.12, 0.11, 0.10]
    }
}

/// Light per second of exposure at gain 1 that puts 18% grey at 18% of full scale at 10 ms.
const CHART_LIGHT: f32 = 100.0;

impl SimScene for ChartScene {
    fn frame(&self, index: u64, width: u32, height: u32, field: [f32; 4]) -> Arc<SceneFrame> {
        let key = field.map(f32::to_bits);
        let base = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            match &*cache {
                Some((w, h, k, base)) if (*w, *h, *k) == (width, height, key) => Arc::clone(base),
                _ => {
                    let base = Arc::new(self.render_static(width, height, field));
                    *cache = Some((width, height, key, Arc::clone(&base)));
                    base
                }
            }
        };
        // The moving square: across and back in 8 s at 30 fps (240 frames).
        let phase = (index % 240) as f32 / 120.0;
        let pos = if phase < 1.0 { phase } else { 2.0 - phase };
        let (sx, sy, side) = (0.05 + 0.8 * pos, 0.86, 0.08);
        let to_x = |u: f32| {
            ((u - field[0]) / (field[2] - field[0]) * width as f32).clamp(0.0, width as f32) as u32
        };
        let to_y = |v: f32| {
            ((v - field[1]) / (field[3] - field[1]) * height as f32).clamp(0.0, height as f32)
                as u32
        };
        let spot = Spot {
            x0: to_x(sx),
            x1: to_x(sx + side * 0.625),
            y0: to_y(sy),
            y1: to_y(sy + side),
            light: [0.9 * CHART_LIGHT * self.light; 3],
        };
        Arc::new(SceneFrame::Rgb {
            width,
            height,
            light: base,
            spot: (spot.x0 < spot.x1 && spot.y0 < spot.y1).then_some(spot),
        })
    }
}

impl ChartScene {
    fn render_static(&self, width: u32, height: u32, field: [f32; 4]) -> Vec<[f32; 3]> {
        let scale = CHART_LIGHT * self.light;
        let mut out = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            let v = field[1] + (field[3] - field[1]) * (y as f32 + 0.5) / height as f32;
            for x in 0..width {
                let u = field[0] + (field[2] - field[0]) * (x as f32 + 0.5) / width as f32;
                out.push(Self::reflectance(u, v).map(|r| r * scale));
            }
        }
        out
    }
}

/// An even grey field (`grey` reflectance, lit as the chart).
pub struct FlatScene {
    grey: f32,
    light: f32,
}

impl FlatScene {
    pub fn new(grey: f32, light: f32) -> Self {
        Self { grey, light }
    }
}

impl SimScene for FlatScene {
    fn frame(&self, _index: u64, width: u32, height: u32, _field: [f32; 4]) -> Arc<SceneFrame> {
        let l = self.grey * CHART_LIGHT * self.light;
        Arc::new(SceneFrame::Rgb {
            width,
            height,
            light: Arc::new(vec![[l; 3]; (width * height) as usize]),
            spot: None,
        })
    }
}

/// How a frame is written to memory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Readout {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: usize,
    pub(crate) packing: RawPacking,
    pub(crate) bits: u8,
    /// Colour filter order of the unflipped array.
    pub(crate) order: ColorFilter,
    /// Black level as a fraction of full scale.
    pub(crate) black: f32,
}

/// One frame's exposure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Exposure {
    pub(crate) seconds: f32,
    pub(crate) gain: f32,
    pub(crate) hflip: bool,
    pub(crate) vflip: bool,
    /// Noise on (a seed for it), or off (`None`: exact values, for tests).
    pub(crate) noise: Option<u64>,
}

/// The colour (0 red, 1 green, 2 blue) of array pixel `(x, y)` for colour filter `order`.
fn colour_at(order: ColorFilter, x: u32, y: u32) -> usize {
    let (odd_x, odd_y) = (x & 1 == 1, y & 1 == 1);
    // The 2x2 cell: top left, top right, bottom left, bottom right.
    let cell = match order {
        ColorFilter::Bggr => [2, 1, 1, 0],
        ColorFilter::Gbrg => [1, 2, 0, 1],
        ColorFilter::Grbg => [1, 0, 2, 1],
        ColorFilter::Rggb => [0, 1, 1, 2],
        ColorFilter::Mono => [1, 1, 1, 1],
    };
    cell[usize::from(odd_y) * 2 + usize::from(odd_x)]
}

/// A uniform value in [-0.5, 0.5) from a pixel, a frame and a seed.
fn hash_noise(x: u32, y: u32, seed: u64) -> f32 {
    let mut h = seed ^ (u64::from(x) << 32 | u64::from(y)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    (h >> 40) as f32 / (1u64 << 24) as f32 - 0.5
}

/// The light on pixel `(px, py)` of a `w`×`h` frame for `colour`: the scene scaled to the frame
/// (a recording of another size), a mosaic's nearest sample of that colour.
fn light_at(scene: &SceneFrame, px: u32, py: u32, colour: usize, (w, h): (u32, u32)) -> f32 {
    match scene {
        SceneFrame::Rgb {
            width,
            height,
            light,
            spot,
        } => {
            let x = (px * width / w).min(width - 1);
            let y = (py * height / h).min(height - 1);
            match spot {
                Some(s) if (s.x0..s.x1).contains(&x) && (s.y0..s.y1).contains(&y) => {
                    s.light[colour]
                }
                _ => light[(y * width + x) as usize][colour],
            }
        }
        SceneFrame::Mosaic {
            width,
            height,
            order,
            light,
        } => {
            let (sx, sy) = (
                (px * width / w).min(width - 1),
                (py * height / h).min(height - 1),
            );
            let (x0, y0) = (sx & !1, sy & !1);
            for (dx, dy) in [(sx & 1, sy & 1), (0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = ((x0 + dx).min(width - 1), (y0 + dy).min(height - 1));
                if *order == ColorFilter::Mono || colour_at(*order, x, y) == colour {
                    return light[(y * width + x) as usize];
                }
            }
            0.0
        }
    }
}

/// Exposes `scene` for one frame into `out` (a frame buffer of `readout`'s format).
pub(crate) fn render(out: &mut [u8], scene: &SceneFrame, readout: &Readout, e: &Exposure) {
    let (w, h) = (readout.width, readout.height);
    let full = ((1u32 << readout.bits) - 1) as f32;
    let k = e.seconds * e.gain;
    let (base, span) = (readout.black * full, (1.0 - readout.black) * full);
    // Shot noise grows with the signal; read noise is a floor (both scaled by gain).
    let (shot, read) = (0.00002 * e.gain, 0.000004 * e.gain * e.gain);
    // Noise: a fast generator per row, seeded from the row and the frame.
    let mut rng = 0u32;
    let sample = |v: f32, rng: &mut u32| -> u16 {
        let mut v = v * k;
        if e.noise.is_some() {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 17;
            *rng ^= *rng << 5;
            let u = (*rng >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            v += u * (shot * v.max(0.0) + read).sqrt() * 3.46;
        }
        (base + v.clamp(0.0, 1.0) * span).round().min(full) as u16
    };
    // The colour of each 2x2 cell position.
    let cell = [0, 1, 2, 3].map(|i| colour_at(readout.order, i & 1, i >> 1));
    let mut row = vec![0u16; w as usize];
    for y in 0..h {
        let py = if e.vflip { h - 1 - y } else { y };
        let cells = &cell[(py as usize & 1) * 2..][..2];
        if let Some(seed) = e.noise {
            rng = (hash_noise(0, py, seed).to_bits() | 1).wrapping_mul(0x9E37_79B9);
        }
        match scene {
            // The common case: a scene of the frame's size, read straight.
            SceneFrame::Rgb {
                width,
                height,
                light,
                spot,
            } if (*width, *height) == (w, h) => {
                let line = &light[(py * w) as usize..][..w as usize];
                let spot = spot.filter(|s| (s.y0..s.y1).contains(&py));
                for (x, o) in row.iter_mut().enumerate() {
                    let px = if e.hflip { w - 1 - x as u32 } else { x as u32 };
                    let c = cells[px as usize & 1];
                    let v = match spot {
                        Some(s) if (s.x0..s.x1).contains(&px) => s.light[c],
                        _ => line[px as usize][c],
                    };
                    *o = sample(v, &mut rng);
                }
            }
            _ => {
                for (x, o) in row.iter_mut().enumerate() {
                    let px = if e.hflip { w - 1 - x as u32 } else { x as u32 };
                    let c = cells[px as usize & 1];
                    *o = sample(light_at(scene, px, py, c, (w, h)), &mut rng);
                }
            }
        }
        let start = y as usize * readout.stride;
        let Some(line) = out.get_mut(start..start + readout.stride) else {
            return;
        };
        pack(line, &row, readout.packing, readout.bits);
    }
}

/// Writes samples into one line of `packing`.
fn pack(line: &mut [u8], row: &[u16], packing: RawPacking, bits: u8) {
    match packing {
        RawPacking::Bits8 => {
            for (o, v) in line.iter_mut().zip(row) {
                *o = (*v >> bits.saturating_sub(8)) as u8;
            }
        }
        RawPacking::Csi2Packed10 => {
            for (o, px) in line.as_chunks_mut::<5>().0.iter_mut().zip(row.chunks(4)) {
                let mut low = 0u8;
                for (i, v) in px.iter().enumerate() {
                    o[i] = (*v >> 2) as u8;
                    low |= ((*v & 3) as u8) << (2 * i);
                }
                o[4] = low;
            }
        }
        RawPacking::Csi2Packed12 => {
            for (o, px) in line.as_chunks_mut::<3>().0.iter_mut().zip(row.chunks(2)) {
                let (a, b) = (px[0], px.get(1).copied().unwrap_or(0));
                o[0] = (a >> 4) as u8;
                o[1] = (b >> 4) as u8;
                o[2] = ((a & 0xf) | ((b & 0xf) << 4)) as u8;
            }
        }
        RawPacking::Unpacked16 => {
            for (o, v) in line.as_chunks_mut::<2>().0.iter_mut().zip(row) {
                o.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readout(packing: RawPacking, stride: usize) -> Readout {
        Readout {
            width: 4,
            height: 2,
            stride,
            packing,
            bits: 10,
            order: ColorFilter::Bggr,
            black: 0.0,
        }
    }

    /// A frame twice as long is twice as bright, and gain does the same.
    #[test]
    fn exposure_and_gain_scale_the_frame() {
        let scene = FlatScene::new(0.18, 1.0).frame(0, 4, 2, [0.0, 0.0, 1.0, 1.0]);
        let r = readout(RawPacking::Unpacked16, 8);
        let mut a = vec![0u8; 16];
        let mut b = vec![0u8; 16];
        let e = |s: f32, g: f32| Exposure {
            seconds: s,
            gain: g,
            hflip: false,
            vflip: false,
            noise: None,
        };
        render(&mut a, &scene, &r, &e(0.010, 1.0));
        render(&mut b, &scene, &r, &e(0.010, 2.0));
        let v = |d: &[u8]| u16::from_le_bytes([d[0], d[1]]);
        // 18% grey at 10 ms, gain 1: 18% of 1023.
        assert_eq!(v(&a), 184);
        assert_eq!(v(&b), 368);
    }

    /// Flipped, the array is read out mirrored: the colour filter order of the frame follows.
    #[test]
    fn flips_change_the_order_of_the_colours() {
        let red = Arc::new(SceneFrame::Rgb {
            width: 4,
            height: 2,
            light: Arc::new(vec![[100.0, 0.0, 0.0]; 8]),
            spot: None,
        });
        let r = readout(RawPacking::Unpacked16, 8);
        let mut out = vec![0u8; 16];
        let e = Exposure {
            seconds: 0.005,
            gain: 1.0,
            hflip: false,
            vflip: false,
            noise: None,
        };
        render(&mut out, &red, &r, &e);
        let px = |d: &[u8], x: usize, y: usize| {
            u16::from_le_bytes([d[y * 8 + 2 * x], d[y * 8 + 2 * x + 1]])
        };
        // BGGR: red at the bottom right of each cell.
        assert!(px(&out, 1, 1) > 0 && px(&out, 0, 0) == 0);
        render(
            &mut out,
            &red,
            &r,
            &Exposure {
                hflip: true,
                vflip: true,
                ..e
            },
        );
        // Both flips: RGGB, red at the top left.
        assert!(px(&out, 0, 0) > 0 && px(&out, 1, 1) == 0);
    }

    #[test]
    fn packed_raw10_holds_the_samples() {
        let mut line = [0u8; 5];
        pack(
            &mut line,
            &[0x3ff, 0x001, 0x200, 0x155],
            RawPacking::Csi2Packed10,
            10,
        );
        assert_eq!(line, [0xff, 0x00, 0x80, 0x55, 0b01_00_01_11]);
    }
}

#[cfg(test)]
mod speed {
    use super::*;

    /// Render time of a 1280x800 CSI-2 RAW10 frame of the chart with noise (`cargo test --release
    /// -p styx-native render_speed -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn render_speed() {
        let (w, h) = (1280, 800);
        let scene = ChartScene::new(1.0).frame(0, w, h, [0.0, 0.0, 1.0, 1.0]);
        let r = Readout {
            width: w,
            height: h,
            stride: 1600,
            packing: RawPacking::Csi2Packed10,
            bits: 10,
            order: ColorFilter::Bggr,
            black: 0.06,
        };
        let mut out = vec![0u8; 1600 * 800];
        for noise in [None, Some(1)] {
            let e = Exposure {
                seconds: 0.01,
                gain: 2.0,
                hflip: false,
                vflip: false,
                noise,
            };
            let t = std::time::Instant::now();
            for _ in 0..20 {
                render(&mut out, &scene, &r, &e);
            }
            let chart = std::time::Instant::now();
            let scenes = ChartScene::new(1.0);
            for i in 0..20 {
                std::hint::black_box(scenes.frame(i, w, h, [0.0, 0.0, 1.0, 1.0]));
            }
            eprintln!(
                "noise {}: render {:.2} ms, chart frame {:.2} ms (the first one drawn)",
                noise.is_some(),
                (chart - t).as_secs_f64() * 1e3 / 20.0,
                chart.elapsed().as_secs_f64() * 1e3 / 20.0
            );
        }
    }
}

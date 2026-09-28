//! A synthetic sensor and scene for testing convergence without hardware.
//!
//! The model:
//!
//! * [`Scene`]: illuminance (lux) and colour temperature over frames (piecewise-linear in the
//!   frame number, so steps and ramps are easy), mains flicker, and a fixed grid of surface
//!   reflectances (mostly greys with some coloured patches).
//! * [`SensorModel`]: pixel value = reflectance × illuminant colour (the sensor's R/G, B/G for
//!   the temperature, from its CT curve) × lux × exposure × gain × responsivity, averaged over
//!   the flicker waveform during the exposure, plus shot and read noise, clipped at full scale.
//!   Exposure is quantised to whole lines and limited by the frame duration.
//! * Control timing like `styx-sensor`'s `ControlScheduler`: each control is written at the
//!   frame start `delay` frames before the frame it is for, and requests made while processing
//!   frame `F` can first be written at the start of frame `F + 2`. Requests that arrive too late
//!   land late and are counted ([`Simulation::late_landings`]).
//!
//! Everything is deterministic: noise comes from a seeded generator without libm calls.

mod metrics;

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::CameraConfig;
use crate::frame::{Controls, FrameMetadata};
use crate::params::Params;
use crate::pipeline::Pipeline;
use crate::pwl::Pwl;
use crate::stats::{Statistics, StatsAccumulator};

pub use metrics::{Convergence, convergence};

/// A small deterministic generator (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seeded.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Approximately normal, mean 0, variance 1 (Irwin-Hall of 4).
    pub fn normal(&mut self) -> f64 {
        let s: f64 = (0..4).map(|_| self.uniform()).sum();
        (s - 2.0) * 3f64.sqrt()
    }
}

/// Mains flicker of the light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneFlicker {
    /// Light modulation frequency (100 Hz for 50 Hz mains).
    pub hz: f64,
    /// Modulation depth, 0 to 1.
    pub depth: f64,
}

/// The scene over time.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// Illuminance by frame number.
    pub lux: Pwl,
    /// Colour temperature (K) by frame number.
    pub ct: Pwl,
    /// Flicker, if any.
    pub flicker: Option<SceneFlicker>,
    /// Reflectance per zone (r, g, b); resampled to the sensor's zone grid.
    pub reflectance: Vec<[f64; 3]>,
    /// Grid of `reflectance` (width, height).
    pub grid: (u32, u32),
}

impl Scene {
    /// A constant scene with the default surfaces.
    pub fn constant(lux: f64, ct: f64) -> Self {
        let (reflectance, grid) = Self::default_surfaces();
        Self {
            lux: Pwl::constant(lux),
            ct: Pwl::constant(ct),
            flicker: None,
            reflectance,
            grid,
        }
    }

    /// A value stepping from `from` to `to` at frame `at`.
    pub fn step(at: u64, from: f64, to: f64) -> Pwl {
        let at = at as f64;
        Pwl::new(vec![(at - 0.5, from), (at, to)]).unwrap_or_else(|_| Pwl::constant(to))
    }

    /// Greys from 5% to 60% reflectance in a diagonal ramp, with every fifth zone a saturated
    /// colour (red, green, blue, yellow, cyan, magenta in turn).
    pub fn default_surfaces() -> (Vec<[f64; 3]>, (u32, u32)) {
        let (w, h) = (16u32, 12u32);
        let colours = [
            [0.45, 0.1, 0.08],
            [0.1, 0.35, 0.1],
            [0.08, 0.1, 0.4],
            [0.5, 0.45, 0.08],
            [0.1, 0.4, 0.45],
            [0.4, 0.1, 0.4],
        ];
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if i % 5 == 2 {
                    out.push(colours[(i / 5) % colours.len()]);
                } else {
                    let g = 0.05 + 0.55 * f64::from(x + y) / f64::from(w + h - 2);
                    out.push([g; 3]);
                }
            }
        }
        (out, (w, h))
    }

    /// Mean light over an exposure `[t0, t0 + t]` relative to the steady level.
    fn flicker_factor(&self, t0: f64, t: f64) -> f64 {
        let Some(f) = self.flicker else { return 1.0 };
        let w = 2.0 * std::f64::consts::PI * f.hz;
        if t <= 0.0 {
            return 1.0 + f.depth * (w * t0).cos();
        }
        1.0 + f.depth * ((w * (t0 + t)).sin() - (w * t0).sin()) / (w * t)
    }
}

/// The sensor model.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorModel {
    /// Statistics zones (width, height).
    pub zones: (u32, u32),
    /// Pixels sampled per zone.
    pub samples_per_zone: u32,
    /// Histogram bins.
    pub histogram_bins: usize,
    /// The sensor's true grey response `[ct, r/g, b/g]`.
    pub ct_curve: Vec<[f64; 3]>,
    /// Normalised green value for reflectance 1 at 1 lux, 1 s, gain 1.
    pub responsivity: f64,
    /// Shot noise variance per unit signal.
    pub shot_noise: f64,
    /// Read noise standard deviation.
    pub read_noise: f64,
    /// Relative texture within a zone (uniform ±half this).
    pub texture: f64,
    /// Line time: exposures are whole lines.
    pub line_time: Duration,
    /// Noise seed.
    pub seed: u64,
}

impl Default for SensorModel {
    fn default() -> Self {
        Self {
            zones: (16, 12),
            samples_per_zone: 16,
            histogram_bins: 128,
            // Raspberry Pi's imx219 CT curve (imx219.json, BSD-2-Clause, Raspberry Pi Ltd).
            ct_curve: vec![
                [2860.0, 0.9514, 0.4156],
                [2960.0, 0.9289, 0.4372],
                [3603.0, 0.8305, 0.5251],
                [4650.0, 0.6756, 0.6433],
                [5858.0, 0.6193, 0.6807],
                [7580.0, 0.5019, 0.7495],
            ],
            responsivity: 0.25,
            shot_noise: 2e-4,
            read_noise: 1e-3,
            texture: 0.2,
            line_time: Duration::from_micros(20),
            seed: 1,
        }
    }
}

impl SensorModel {
    /// Raw (r, g, b) of a white surface under a colour temperature.
    pub fn illuminant(&self, ct: f64) -> (f64, f64, f64) {
        let pts = |i: usize| {
            self.ct_curve
                .iter()
                .map(|p| (p[0], p[i]))
                .collect::<Vec<_>>()
        };
        let r = Pwl::new(pts(1)).map_or(1.0, |p| p.eval_clamped(ct));
        let b = Pwl::new(pts(2)).map_or(1.0, |p| p.eval_clamped(ct));
        (r, 1.0, b)
    }
}

/// One simulated frame.
#[derive(Debug, Clone)]
pub struct SimFrame {
    /// What produced the frame.
    pub meta: FrameMetadata,
    /// Its statistics.
    pub stats: Statistics,
    /// The pipeline's output for it.
    pub params: Params,
    /// Scene illuminance.
    pub lux: f64,
    /// Scene colour temperature.
    pub ct: f64,
    /// Mean raw luma (what AE meters, before digital gain).
    pub raw_y: f64,
}

const EXPOSURE: usize = 0;
const GAIN: usize = 1;
const DURATION: usize = 2;

/// A running simulation. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Simulation {
    /// The sensor.
    pub sensor: SensorModel,
    /// The scene.
    pub scene: Scene,
    /// Application controls used from the next frame on.
    pub controls: Controls,
    config: CameraConfig,
    rng: Rng,
    frame: u64,
    time: f64,
    incoming: Vec<(u64, [f64; 3])>,
    pending: [BTreeMap<u64, f64>; 3],
    landed: [BTreeMap<u64, f64>; 3],
    late: u64,
}

impl Simulation {
    /// A simulation starting at 1 ms, gain 1 (see [`Simulation::start_with`]).
    pub fn new(sensor: SensorModel, scene: Scene, config: &CameraConfig) -> Self {
        let rng = Rng::new(sensor.seed);
        let fd = config.frame_duration_limits.0.as_secs_f64();
        let mut s = Self {
            sensor,
            scene,
            controls: Controls::default(),
            config: config.clone(),
            rng,
            frame: 0,
            time: 0.0,
            incoming: Vec::new(),
            pending: Default::default(),
            landed: Default::default(),
            late: 0,
        };
        s.start_with(0.001, 1.0, fd);
        s
    }

    /// Values in effect from frame 0 (written before streaming), e.g. from the pipeline's
    /// start-up parameters.
    pub fn start_with(&mut self, exposure: f64, gain: f64, frame_duration: f64) {
        for (c, v) in [
            (EXPOSURE, exposure),
            (GAIN, gain),
            (DURATION, frame_duration),
        ] {
            self.landed[c].insert(0, v);
        }
    }

    /// Requests that landed later than asked.
    pub fn late_landings(&self) -> u64 {
        self.late
    }

    fn delay(&self, c: usize) -> u64 {
        let d = &self.config.delays;
        u64::from([d.exposure, d.analogue_gain, d.frame_duration][c])
    }

    /// Frame start: issue what is due, as the control scheduler would, then accept the
    /// requests made while processing the previous frame.
    fn frame_start(&mut self) {
        let f = self.frame;
        for c in 0..3 {
            let lands = f + self.delay(c);
            let due: Vec<u64> = self.pending[c].range(..=lands).map(|(k, _)| *k).collect();
            if let Some(&last) = due.last() {
                let v = self.pending[c][&last];
                if last < lands {
                    self.late += 1;
                }
                for k in due {
                    self.pending[c].remove(&k);
                }
                self.landed[c].insert(lands, v);
            }
        }
        for (target, v) in self.incoming.drain(..) {
            for (c, value) in v.iter().enumerate() {
                self.pending[c].insert(target, *value);
            }
        }
    }

    fn value(&self, c: usize) -> f64 {
        self.landed[c]
            .range(..=self.frame)
            .next_back()
            .map_or(0.0, |(_, v)| *v)
    }

    /// Expose the current frame.
    fn expose(&mut self) -> (FrameMetadata, Statistics, f64, f64) {
        let f = self.frame as f64;
        let (lux, ct) = (
            self.scene.lux.eval_clamped(f),
            self.scene.ct.eval_clamped(f),
        );
        let (lo, hi) = self.config.frame_duration_limits;
        let fd = self
            .value(DURATION)
            .clamp(lo.as_secs_f64(), hi.as_secs_f64());
        let line = self.sensor.line_time.as_secs_f64();
        let max_exp = (fd - self.config.exposure_margin.as_secs_f64()).max(line);
        let exposure = ((self.value(EXPOSURE).min(max_exp) / line).floor().max(1.0)) * line;
        let (glo, ghi) = self.config.analogue_gain_limits;
        let gain = self.value(GAIN).clamp(glo, ghi);
        let light = lux
            * self
                .scene
                .flicker_factor(self.time + fd - exposure, exposure);
        let (ir, ig, ib) = self.sensor.illuminant(ct);
        let s = &self.sensor;
        let (zw, zh) = s.zones;
        let (gw, gh) = self.scene.grid;
        let mut acc = StatsAccumulator::new(zw, zh, s.histogram_bins, 1.0);
        let k = light * exposure * gain * s.responsivity;
        for zy in 0..zh {
            for zx in 0..zw {
                let rx = (zx * gw / zw).min(gw - 1);
                let ry = (zy * gh / zh).min(gh - 1);
                let refl = self.scene.reflectance[(ry * gw + rx) as usize];
                for _ in 0..s.samples_per_zone {
                    let tex = 1.0 + s.texture * (self.rng.uniform() - 0.5);
                    let mut px = [refl[0] * ir, refl[1] * ig, refl[2] * ib];
                    for v in &mut px {
                        let clean = *v * tex * k;
                        let sd = (s.shot_noise * clean + s.read_noise * s.read_noise).sqrt();
                        *v = (clean + sd * self.rng.normal()).clamp(0.0, 1.0);
                    }
                    acc.add(zx, zy, px[0], px[1], px[2]);
                }
            }
        }
        let stats = acc.finish();
        let mut meta = FrameMetadata::new(
            self.frame,
            Duration::from_secs_f64(exposure),
            gain,
            Duration::from_secs_f64(fd),
        );
        meta.controls = self.controls.clone();
        self.time += fd;
        (meta, stats, lux, ct)
    }

    /// Run `frames` frames through a prepared pipeline.
    pub fn run(&mut self, pipeline: &mut Pipeline, frames: u64) -> Vec<SimFrame> {
        let mut out = Vec::with_capacity(frames as usize);
        for _ in 0..frames {
            self.frame_start();
            let (meta, stats, lux, ct) = self.expose();
            let params = pipeline.process(&stats, &meta).clone();
            let repeat = out.last().is_some_and(|l: &SimFrame| l.params.sensor == params.sensor);
            if let Some(req) = params.sensor.filter(|_| !repeat) {
                self.incoming.push((
                    req.frame,
                    [
                        req.exposure.as_secs_f64(),
                        req.analogue_gain,
                        req.frame_duration.as_secs_f64(),
                    ],
                ));
            }
            out.push(SimFrame {
                raw_y: stats.mean_luma(),
                meta,
                stats,
                params,
                lux,
                ct,
            });
            self.frame += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_roughly_normal() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.next_u64(), b.next_u64());
        let n = 20000;
        let (mut s, mut s2) = (0.0, 0.0);
        for _ in 0..n {
            let x = a.normal();
            s += x;
            s2 += x * x;
        }
        assert!((s / n as f64).abs() < 0.05);
        assert!((s2 / n as f64 - 1.0).abs() < 0.05);
    }

    #[test]
    fn flicker_cancels_over_whole_periods() {
        let mut sc = Scene::constant(100.0, 5000.0);
        sc.flicker = Some(SceneFlicker {
            hz: 100.0,
            depth: 0.8,
        });
        for t0 in [0.0, 0.0013, 0.0071] {
            assert!((sc.flicker_factor(t0, 0.02) - 1.0).abs() < 1e-9);
        }
        assert!((sc.flicker_factor(0.0, 0.0025) - 1.0).abs() > 0.1);
    }

    #[test]
    fn requests_land_on_the_requested_frame() {
        let config = CameraConfig::default();
        let scene = Scene::constant(100.0, 5000.0);
        let mut sim = Simulation::new(SensorModel::default(), scene, &config);
        // While processing frame 0: ask for frame 0 + 2 + 2 = 4.
        sim.frame_start();
        sim.incoming.push((4, [0.004, 2.0, 0.02]));
        for f in 1..7 {
            sim.frame = f;
            sim.frame_start();
            let v = (sim.value(EXPOSURE), sim.value(GAIN), sim.value(DURATION));
            if f < 4 {
                assert_eq!((v.0, v.1), (0.001, 1.0), "frame {f}");
            } else {
                assert_eq!(v, (0.004, 2.0, 0.02), "frame {f}");
            }
        }
        assert_eq!(sim.late_landings(), 0);
    }
}

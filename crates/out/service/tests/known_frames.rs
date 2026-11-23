//! Raw frames of a known light and a known level, through both statistics routes (the back end's
//! input pass, `BayerStats`, and the soft ISP's own) and the 3A controller with the full module
//! tuning, as the service runs them. Each case states what the light or the level asks for.
//!
//! The sim's frames before these were black or flat grey: a grey on the tuning's curve never
//! left the curve's reach, and a black frame never showed the statistics' scale. So AE settling
//! dark (the soft route's sums handed over at 0..4095) and AWB answering from its prior alone (a
//! scene beyond the curve's warm end) both passed the sim and failed on the board.

use std::time::Duration;

use styx_aaa::{
    BayerStats, BayerStatsConfig, CameraConfig, Cfa, FrameMetadata, Params, Statistics, Tuning,
};
use styx_controller::Controller;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_settings::IspSettings;
use styx_soft::{Options, SoftIspStage, StatsConfig};

const W: u32 = 1280;
const H: u32 = 800;
/// RAW10 black level, the tuning's (4172.8 of 65536 is 65.1 of 1023).
const BLACK: f64 = 4172.8 / 65536.0;
const FRAME: Duration = Duration::from_nanos(8_335_625);

fn tuning() -> Tuning {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/tuning");
    let found = styx_tuning::TuningSearch::new([std::path::PathBuf::from(dir)], None)
        .find(Some("ov9782.toml"));
    assert!(
        found.describe().contains("ov9782.toml"),
        "{}",
        found.describe()
    );
    found.tuning
}

fn controller(tuning: &Tuning) -> Controller {
    Controller::new(
        tuning,
        CameraConfig {
            exposure_limits: (Duration::from_micros(9), Duration::from_micros(8300)),
            frame_duration_limits: (FRAME, FRAME),
            analogue_gain_limits: (1.0, 15.938),
            ..CameraConfig::default()
        },
    )
    .unwrap()
}

/// A frame: each pixel's linear level (0..1 above black) per channel, from `light(x, y)`.
struct Raw {
    /// 10-bit samples, row-major.
    px: Vec<u16>,
}

impl Raw {
    fn new(light: impl Fn(u32, u32) -> [f64; 3]) -> Self {
        let black = BLACK * 1023.0;
        let mut px = vec![0u16; (W * H) as usize];
        for y in 0..H {
            for x in 0..W {
                let [r, g, b] = light(x, y);
                // BGGR: even rows B G, odd rows G R.
                let v = match (y % 2, x % 2) {
                    (0, 0) => b,
                    (1, 1) => r,
                    _ => g,
                };
                px[(y * W + x) as usize] =
                    (black + v * (1023.0 - black)).round().clamp(0.0, 1023.0) as u16;
            }
        }
        Self { px }
    }

    /// The back end's route: counted in the input pass.
    fn pisp(&self) -> Statistics {
        let mut bs = BayerStats::new(
            BayerStatsConfig::new(W, H, Cfa::Bggr, 10),
            (BLACK * 1023.0).round() as u32,
        );
        for y in 0..H as usize {
            bs.add_row(y, &self.px[y * W as usize..][..W as usize]);
        }
        let mut out = Statistics::default();
        bs.finish_into(&mut out);
        out
    }

    /// The frame as the camera gives it (CSI-2 RAW10), for the soft ISP.
    fn lease(&self, pool: &FramePool<()>) -> FrameLease<()> {
        let code = FourCc::new(*b"pBAA");
        let layouts = packed_layouts(code, W, H, None, None).unwrap();
        let stride = layouts[0].stride;
        let fmt = MediaFormat::with_defaults(code, Resolution::new(W, H).unwrap());
        let mut lease = pool.acquire(FrameEntry::new(1, 1, fmt, layouts)).unwrap();
        {
            let mut w = lease.write().unwrap();
            let mut plane = w.plane(0).unwrap();
            for y in 0..H as usize {
                let row = plane.row_bytes_mut(y, stride).unwrap();
                let src = &self.px[y * W as usize..][..W as usize];
                for (g, out) in row
                    .as_chunks_mut::<5>()
                    .0
                    .iter_mut()
                    .take(W as usize / 4)
                    .enumerate()
                {
                    let s = &src[g * 4..][..4];
                    for i in 0..4 {
                        out[i] = (s[i] >> 2) as u8;
                    }
                    out[4] = s
                        .iter()
                        .enumerate()
                        .fold(0, |a, (i, v)| a | ((*v & 3) as u8) << (2 * i));
                }
            }
        }
        lease
    }
}

/// The soft ISP's route, as the service builds it (32x32 zones, every other quad row), with
/// `settings` in force (the loop's, so its lens shading is applied before the statistics).
struct Soft {
    stage: SoftIspStage,
    raw: FramePool<()>,
    out: FramePool<()>,
}

impl Soft {
    fn new() -> Self {
        let code = FourCc::new(*b"pBAA");
        let layouts = packed_layouts(code, W, H, None, None).unwrap();
        let raw = FramePool::host(1, BufferSize::new(layouts.span().unwrap(), 0)).unwrap();
        let ol = packed_layouts(FourCc::NV12, W, H, None, None).unwrap();
        let out = FramePool::host(1, BufferSize::new(ol.span().unwrap(), 0)).unwrap();
        let options = Options {
            stats: Some(StatsConfig {
                zones_x: 32,
                zones_y: 32,
                histogram_bins: 128,
                row_step: 2,
                ..StatsConfig::default()
            }),
            ..Options::default()
        };
        let stage = SoftIspStage::new(code, W, H, &IspSettings::neutral(BLACK), options).unwrap();
        Self { stage, raw, out }
    }

    fn stats(&mut self, frame: &Raw, settings: &IspSettings) -> Statistics {
        self.stage.apply(settings).unwrap();
        let raw = frame.lease(&self.raw);
        let ol = packed_layouts(FourCc::NV12, W, H, None, None).unwrap();
        let fmt = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(W, H).unwrap());
        let mut out = self.out.acquire(FrameEntry::new(1, 1, fmt, ol)).unwrap();
        let mut stats = Statistics::default();
        self.stage
            .process(&raw, &mut out, Some(&mut stats))
            .unwrap();
        stats
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Pisp,
    Soft,
}

/// `frames` steps of the loop on `frame` (fixed: the light does not follow the requests, so the
/// controller's statistics are the frame's own), exposed `exposure` at 1x. The last step's
/// params.
fn run(route: Route, frame: &Raw, exposure: Duration, frames: u64) -> Params {
    let tuning = tuning();
    let mut c = controller(&tuning);
    let mut soft = (route == Route::Soft).then(Soft::new);
    let mut settings = IspSettings::neutral(BLACK);
    let mut meta = FrameMetadata::new(0, exposure, 1.0, FRAME);
    let mut last = None;
    for f in 0..frames {
        meta.frame = f;
        let stats = match soft.as_mut() {
            Some(s) => s.stats(frame, &settings),
            None => frame.pisp(),
        };
        let step = c.process(&stats, &meta).unwrap();
        settings.clone_from(step.isp);
        last = Some(step.params.clone());
    }
    last.unwrap()
}

/// The tuning's lens shading for the controller's first frame (its calibrated tables): the
/// light a lens with exactly that shading lets through is divided by them.
fn shading() -> styx_aaa::LensShading {
    run(
        Route::Pisp,
        &Raw::new(|_, _| [0.05; 3]),
        Duration::from_millis(5),
        1,
    )
    .lens_shading
    .expect("the tuning has lens shading")
}

/// The tables at pixel (x, y), between the cells' centres (as the ISPs read them).
fn table_at(t: &styx_aaa::LensShading, x: u32, y: u32) -> [f64; 3] {
    let at = |p: u32, size: u32, cells: u32| {
        let c = ((f64::from(p) + 0.5) * f64::from(cells) / f64::from(size) - 0.5)
            .clamp(0.0, f64::from(cells - 1));
        let i = (c as u32).min(cells.saturating_sub(2));
        (i as usize, c - f64::from(i))
    };
    let ((x0, fx), (y0, fy)) = (at(x, W, t.width), at(y, H, t.height));
    let w = t.width as usize;
    [&t.r, &t.g, &t.b].map(|v| {
        let p = |dx: usize, dy: usize| v[(y0 + dy) * w + x0 + dx];
        (p(0, 0) * (1.0 - fx) + p(1, 0) * fx) * (1.0 - fy)
            + (p(0, 1) * (1.0 - fx) + p(1, 1) * fx) * fy
    })
}

/// A grey scene under a light on the tuning's curve (4650 K: a grey reads R/G 0.6756, B/G 0.6433),
/// through a lens shaded as the tuning says: both routes find that light, and the gains that make
/// the grey grey.
#[test]
fn a_known_light_on_the_curve_gives_its_gains_on_both_routes() {
    let lens = shading();
    let frame = Raw::new(|x, y| {
        let t = table_at(&lens, x, y);
        let g = 0.12;
        [g * 0.6756 / t[0], g / t[1], g * 0.6433 / t[2]]
    });
    for route in [Route::Pisp, Route::Soft] {
        let p = run(route, &frame, Duration::from_millis(5), 30);
        let [r, _, b] = p.colour_gains;
        println!(
            "{route:?}: ct {:.0} gains r {r:.3} b {b:.3} outside {:?}",
            p.colour_temperature, p.awb.outside
        );
        assert!(p.awb.outside.is_none(), "{route:?}: on the curve");
        assert!(
            (p.colour_temperature - 4650.0).abs() < 250.0,
            "{route:?}: {:.0} K, the light is 4650 K",
            p.colour_temperature
        );
        assert!(
            (r * 0.6756 - 1.0).abs() < 0.04,
            "{route:?}: red gain {r:.3}, the grey needs {:.3}",
            1.0 / 0.6756
        );
        assert!(
            (b * 0.6433 - 1.0).abs() < 0.04,
            "{route:?}: blue gain {b:.3}, the grey needs {:.3}",
            1.0 / 0.6433
        );
    }
}

/// The board's scene (3A proof 1's raw frame): a grey reading R/G 1.37 and B/G 0.22 in the
/// centre, far beyond the curve's warm end (0.95 / 0.42), dim (2.5% of full scale at 5 ms), through
/// a lens that darkens the corners as the tuning's luminance table says but shades no colour (the
/// tuning's colour tables then redden the corners further, as on the board). The prior alone chose
/// 6155 K there and gains [1.70, 1, 1.49]: the wrong way. Both routes now go the right way (red
/// down to the curve's end, blue up) and say the scene is outside the tuning.
#[test]
fn a_scene_beyond_the_curves_warm_end_goes_the_right_way_and_says_so() {
    let lens = shading();
    let frame = Raw::new(|x, y| {
        let t = table_at(&lens, x, y)[1];
        let g = 0.025;
        [g * 1.37 / t, g / t, g * 0.22 / t]
    });
    for route in [Route::Pisp, Route::Soft] {
        let p = run(route, &frame, Duration::from_micros(5035), 30);
        let [r, _, b] = p.colour_gains;
        println!(
            "{route:?}: ct {:.0} gains r {r:.3} b {b:.3} outside {:?}",
            p.colour_temperature, p.awb.outside
        );
        // The mean said is of the zones as the tables leave them: redder than the centre's 1.37,
        // as the colour tables redden the corners the lens did not.
        let (mr, mb) = p
            .awb
            .outside
            .expect("outside the tuning's colour range, said");
        assert!(
            (1.37..1.9).contains(&mr) && (mb - 0.22).abs() < 0.03,
            "{route:?}: the scene's mean {mr:.3}/{mb:.3}"
        );
        assert!(
            r < 1.1,
            "{route:?}: red gain {r:.3} (the scene is red: not up)"
        );
        assert!(
            b > 2.3,
            "{route:?}: blue gain {b:.3} (the scene lacks blue: up)"
        );
        assert!(
            p.colour_temperature < 3000.0,
            "{route:?}: {:.0} K",
            p.colour_temperature
        );
    }
}

/// A grey of known level L at exposure E, 1x, through a lens shaded as the tuning says (L in the
/// centre, darker towards the corners): the tuning's AE asks the top of the histogram for 0.5 (its
/// constraint, the stronger push on a flat grey: the mean target is 0.16), so it asks for
/// E x 0.5 / L, within its limits. The same on both routes; the soft route's sums at 0..4095 asked
/// for about 4000 times too little, and its histogram, binned after the gains, about 5 times.
#[test]
fn a_known_level_gets_the_exposure_it_needs_on_both_routes() {
    let level = 0.05;
    let exposure = Duration::from_millis(5);
    let lens = shading();
    let frame = Raw::new(|x, y| table_at(&lens, x, y).map(|t| level / t));
    let want = exposure.as_secs_f64() * 0.5 / level;
    for route in [Route::Pisp, Route::Soft] {
        let p = run(route, &frame, exposure, 6);
        let got = p.ae.target_exposure;
        println!("{route:?}: target total exposure {got:.5} s, want {want:.5} s");
        assert!(
            (got / want - 1.0).abs() < 0.1,
            "{route:?}: AE aims at {got:.5} s (exposure x gain), the level asks {want:.5} s"
        );
    }
}

/// Frames until the loop settles, closed: each frame is exposed as the loop's newest request that
/// has landed asks (the light follows exposure x gain), and the loop steps on one frame in `every`
/// (`aaa_stats_every`), as the service runs it. From a dark start; then, once settled, the light
/// four times brighter. Returns the frames to settle from the start and after the step.
fn settle(route: Route, every: u64) -> (u64, u64) {
    let tuning = tuning();
    let mut c = controller(&tuning);
    let mut soft = (route == Route::Soft).then(Soft::new);
    let mut settings = IspSettings::neutral(BLACK);
    let lens = shading();
    // A grey under the 4650 K light, `light` of full scale (in the centre) at 1 ms, 1x.
    let at = |light: f64| {
        Raw::new(|x, y| {
            let t = table_at(&lens, x, y);
            [0.6756 * light / t[0], light / t[1], 0.6433 * light / t[2]].map(|v| v.min(1.0))
        })
    };
    let (mut exposure, mut gain) = (Duration::from_millis(1), 1.0);
    let mut landed: Option<styx_aaa::SensorRequest> = None;
    let (mut light, mut since, mut settled_for) = (0.01, 0, 0);
    let mut first = None;
    for step in 0..600u64 {
        let frame = step * every;
        if let Some(r) = landed.filter(|r| r.frame <= frame) {
            (exposure, gain) = (r.exposure, r.analogue_gain);
        }
        let raw = at(light * exposure.as_secs_f64() / 1e-3 * gain);
        let stats = match soft.as_mut() {
            Some(s) => s.stats(&raw, &settings),
            None => raw.pisp(),
        };
        let meta = FrameMetadata::new(frame, exposure, gain, FRAME);
        let out = c.process(&stats, &meta).unwrap();
        if let Some(r) = out.sensor {
            landed = Some(r);
        }
        settings.clone_from(out.isp);
        let p = &out.params;
        let awb_there = (p.colour_temperature - 4650.0).abs() < 300.0;
        settled_for = if p.ae.locked && awb_there {
            settled_for + 1
        } else {
            0
        };
        // Settled: AE locked and AWB on the light, three steps running.
        if settled_for == 3 {
            let took = frame - 2 * every - since;
            match first {
                None => {
                    first = Some(took);
                    (light, since, settled_for) = (light * 4.0, frame + every, 0);
                }
                Some(first) => return (first, took),
            }
        }
    }
    panic!("{route:?}: not settled in 600 steps of {every} frames");
}

/// How long the loop takes to settle at the service's default sampling (one frame in 4 at
/// 120 fps: 30 steps a second, the rate the tunings' speeds are made for): from a dark start, and
/// after the light steps four times brighter. Within a second each, on both routes.
#[test]
fn the_loop_settles_within_a_second_at_the_default_sampling() {
    let every = u64::from(styx_service::StatsSampling::DEFAULT_EVERY);
    for route in [Route::Pisp, Route::Soft] {
        let (start, step) = settle(route, every);
        let s = |frames: u64| frames as f64 / 120.0;
        println!(
            "{route:?}: settled from dark after {start} frames ({} steps) = {:.3} s, after the light x4 \
             {step} frames ({} steps) = {:.3} s, at 120 fps",
            start / every,
            s(start),
            step / every,
            s(step)
        );
        assert!(s(start) < 1.0 && s(step) < 1.0, "{route:?}");
    }
}

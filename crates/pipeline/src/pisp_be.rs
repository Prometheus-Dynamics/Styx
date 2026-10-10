//! The PiSP back end config for a stream of frames, rebuilt only as far as the settings
//! change.
//!
//! The algorithms move a few blocks from frame to frame (black level, white balance and
//! digital gain, CCM, now and then the gamma curve and the lens shading table); the tiles and
//! everything else depend only on the geometry. So the config and its tiles are prepared once
//! (and again only when lens shading is switched on or off, which changes the tiles), and each
//! frame patches the blocks whose register values changed. Lens shading tables are resampled
//! to the back end's grid and gamma curves converted only when they change.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use styx_algo::{LensShading, Pwl};
use styx_pisp::be::BackEnd;
use styx_pisp::uapi::{BeCropConfig, BeLscExtra, BeTilesConfig, ImageFormatConfig, bayer_enable};

use crate::error::{PipelineError, Result};
use crate::isp::{IspSettings, be_lens_shading, gamma_points_into, level16};

/// What [`BeConfigBuilder::update`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeUpdate {
    /// Prepared from the template (first frame, lens shading switched on or off).
    Rebuilt,
    /// Some blocks changed; tiles kept.
    Patched,
    /// The config is the previous frame's.
    Unchanged,
}

/// How often each kind of update happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BeUpdateCounts {
    /// Full prepares.
    pub rebuilt: u64,
    /// Blocks patched.
    pub patched: u64,
    /// Nothing changed.
    pub unchanged: u64,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct BeConfigBuilder {
    template: BackEnd,
    /// Scratch builder whose setters give the blocks' register values.
    work: BackEnd,
    cfg: Box<BeTilesConfig>,
    fresh: bool,
    lsc_on: bool,
    gamma: Option<Pwl>,
    /// The tone curve's points on the 16-bit scale, as the back end takes them (scratch).
    gamma_points: Vec<(u32, u32)>,
    lens_shading: Option<LensShading>,
    counts: BeUpdateCounts,
    /// Temporal denoise buffers exist (see [`Self::enable_tdn`]).
    tdn: bool,
    /// Exposure × analogue gain of the last frame with temporal denoise on (`None`: the next
    /// one starts a new average).
    tdn_last: Option<f64>,
    /// Full prepares so far (extra passes re-prepare when it moves).
    generation: u64,
}

/// Above this exposure ratio between frames the temporal average starts over (as the
/// Raspberry Pi IPA; the ratio register holds up to 4).
const TDN_MAX_RATIO: f64 = 4.0;

fn config_error(e: styx_pisp::be::PrepareError) -> PipelineError {
    PipelineError::Config(format!("back end: {}", e.0))
}

impl BeConfigBuilder {
    /// A builder for frames processed with `template` (e.g. [`crate::isp::be_template`]) plus
    /// the algorithms' settings. Fails if the template does not prepare.
    pub fn new(template: BackEnd) -> Result<Self> {
        let mut work = template.clone();
        let cfg = work.prepare().map_err(config_error)?;
        Ok(Self {
            template,
            work,
            cfg,
            fresh: true,
            lsc_on: false,
            gamma: None,
            gamma_points: Vec::new(),
            lens_shading: None,
            counts: BeUpdateCounts::default(),
            tdn: false,
            tdn_last: None,
            generation: 0,
        })
    }

    /// Temporal denoise buffers of `format` (the input's, see
    /// `styx_pisp::device::BackEndStream::enable_tdn`) are there: frames whose settings ask
    /// for temporal denoise get it from [`Self::update_frame`] on.
    pub fn enable_tdn(&mut self, format: ImageFormatConfig) {
        self.template.set_tdn_format(format);
        self.work.set_tdn_format(format);
        self.tdn = true;
        self.tdn_last = None;
        self.fresh = true;
    }

    /// Output `i` cropped to `crop` of the input (`None`: all of it), from the next frame's
    /// config on (a full prepare). Fails, changing nothing, if the back end cannot make it.
    ///
    /// The temporal average is only updated where a frame's tiles read: a crop that moves
    /// mostly outside what the last frame covered starts it over rather than reading what an
    /// older frame left there.
    pub fn set_output_crop(&mut self, i: usize, crop: Option<BeCropConfig>) -> Result<()> {
        let mut template = self.template.clone();
        template.set_crop(i, crop.unwrap_or_default());
        let next = template.clone().prepare().map_err(config_error)?;
        let (a, b) = (
            crate::pisp_passes::tiles_cover(&self.cfg),
            crate::pisp_passes::tiles_cover(&next),
        );
        let overlap =
            |lo0: u32, hi0: u32, lo1: u32, hi1: u32| hi0.min(hi1).saturating_sub(lo0.max(lo1));
        let shared =
            u64::from(overlap(a.0, a.2, b.0, b.2)) * u64::from(overlap(a.1, a.3, b.1, b.3));
        let area = u64::from(b.2.saturating_sub(b.0)) * u64::from(b.3.saturating_sub(b.1));
        if shared * 2 < area {
            self.reset_tdn();
        }
        self.template = template;
        self.fresh = true;
        Ok(())
    }

    /// The temporal average starts over with the next frame.
    pub fn reset_tdn(&mut self) {
        self.tdn_last = None;
    }

    /// [`Self::update`] for a frame exposed for `exposure` (time × analogue gain, in any
    /// unit), which temporal denoise needs to scale its average.
    pub fn update_frame(&mut self, isp: &IspSettings, exposure: f64) -> Result<BeUpdate> {
        self.update_inner(isp, Some(exposure))
    }

    /// The config for a frame processed with `isp` (no temporal denoise).
    pub fn update(&mut self, isp: &IspSettings) -> Result<BeUpdate> {
        self.update_inner(isp, None)
    }

    /// The temporal denoise config for this frame, and whether it reads the average.
    fn tdn_for(&mut self, isp: &IspSettings, exposure: Option<f64>) -> Option<(f64, bool)> {
        let exposure = exposure.filter(|e| *e > 0.0 && self.tdn && isp.denoise.tdn.is_some());
        let Some(e) = exposure else {
            self.tdn_last = None;
            return None;
        };
        let ratio = self.tdn_last.map(|l| e / l);
        self.tdn_last = Some(e);
        match ratio {
            Some(r) if r < TDN_MAX_RATIO => Some((r, true)),
            _ => Some((1.0, false)),
        }
    }

    fn update_inner(&mut self, isp: &IspSettings, exposure: Option<f64>) -> Result<BeUpdate> {
        let tdn = self.tdn_for(isp, exposure);
        let tdn_cfg = tdn.and_then(|(ratio, read)| isp.be_tdn(ratio, !read).map(|c| (c, read)));
        let ls_changed = self.fresh || self.lens_shading != isp.lens_shading;
        let lsc = if ls_changed {
            isp.lens_shading.as_ref().and_then(be_lens_shading)
        } else {
            None
        };
        let lsc_on = if ls_changed {
            lsc.is_some()
        } else {
            self.lsc_on
        };
        // Blocks switched on or off (other than reading the temporal average) re-prepare.
        let mask = !bayer_enable::TDN_INPUT;
        let mut probe = self.work.clone();
        isp.apply_be_detail(&mut probe);
        probe.set_tdn(tdn_cfg.map(|t| t.0), tdn_cfg.is_some_and(|t| t.1));
        let enables = |g: styx_pisp::uapi::BeGlobalConfig| (g.bayer_enables & mask, g.rgb_enables);
        let blocks_changed = enables(probe.config().global) != enables(self.work.config().global);
        if self.fresh || lsc_on != self.lsc_on || blocks_changed {
            let mut be = self.template.clone();
            isp.apply_be(&mut be);
            be.set_tdn(tdn_cfg.map(|t| t.0), tdn_cfg.is_some_and(|t| t.1));
            self.cfg = be.prepare().map_err(config_error)?;
            self.work = be;
            self.fresh = false;
            self.lsc_on = lsc_on;
            self.gamma.clone_from(&isp.gamma);
            self.lens_shading.clone_from(&isp.lens_shading);
            self.counts.rebuilt += 1;
            self.generation += 1;
            return Ok(BeUpdate::Rebuilt);
        }
        let w = &mut self.work;
        w.set_black_level(level16(isp.black_level));
        isp.apply_be_detail(w);
        w.set_tdn(tdn_cfg.map(|t| t.0), tdn_cfg.is_some_and(|t| t.1));
        let g = isp.channel_gains();
        w.set_wb_gains(g[0], g[1], g[2]);
        w.set_ccm(isp.ccm);
        if self.gamma != isp.gamma {
            gamma_points_into(isp.gamma.as_ref(), &mut self.gamma_points);
            w.set_gamma_curve(&self.gamma_points);
            self.gamma.clone_from(&isp.gamma);
        }
        if ls_changed {
            if let Some(table) = lsc {
                w.set_lsc(table, BeLscExtra::default());
            }
            self.lens_shading.clone_from(&isp.lens_shading);
        }
        let (c, n) = (&mut self.cfg.config, w.config());
        let mut changed = false;
        if c.blc != n.blc {
            c.blc = n.blc;
            changed = true;
        }
        if c.wbg != n.wbg {
            c.wbg = n.wbg;
            changed = true;
        }
        if c.ccm != n.ccm {
            c.ccm = n.ccm;
            changed = true;
        }
        if c.gamma != n.gamma {
            c.gamma = n.gamma;
            changed = true;
        }
        macro_rules! patch {
            ($($f:ident),*) => {$(
                if c.$f != n.$f {
                    c.$f = n.$f;
                    changed = true;
                }
            )*};
        }
        patch!(dpc, geq, sdn, cdn, tdn, sharpen, sh_fc_combine, global);
        // The grid steps were finalised by the last prepare; only the table changes.
        if lsc_on && c.lsc.lut_packed != n.lsc.lut_packed {
            c.lsc.lut_packed = n.lsc.lut_packed;
            changed = true;
        }
        Ok(if changed {
            self.counts.patched += 1;
            BeUpdate::Patched
        } else {
            self.counts.unchanged += 1;
            BeUpdate::Unchanged
        })
    }

    /// The config as of the last [`Self::update`] (the template's before the first).
    pub fn config(&self) -> &BeTilesConfig {
        &self.cfg
    }

    /// How often each kind of update happened.
    pub fn counts(&self) -> BeUpdateCounts {
        self.counts
    }

    /// The builder behind [`Self::config`]: its blocks as last set, its geometry the
    /// template's (extra passes prepare their own geometry from it).
    pub fn work(&self) -> &BackEnd {
        &self.work
    }

    /// Counts the full prepares: a config patched since keeps it (its tiles and enables are
    /// the same).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use styx_algo::{
        CdnParams, DenoiseParams, GeqParams, Params, SdnParams, SharpenParams, TdnParams,
    };
    use styx_pisp::format::{compute_stride_align, formats};
    use styx_pisp::uapi::{BayerOrder, ImageFormatConfig};

    use super::*;
    use crate::isp::be_template;

    fn template() -> BackEnd {
        let fmt = |w: u16, h: u16, format: u32| {
            let mut f = ImageFormatConfig {
                width: w,
                height: h,
                format,
                ..Default::default()
            };
            compute_stride_align(&mut f, 64);
            f
        };
        be_template(
            fmt(1280, 800, formats::BAYER16),
            BayerOrder::Bggr,
            64.0 / 1024.0,
            [
                Some(fmt(1280, 800, formats::NV12)),
                Some(fmt(640, 400, formats::RGB888)),
            ],
        )
        .unwrap()
    }

    fn shading(k: f64) -> LensShading {
        LensShading {
            width: 16,
            height: 12,
            r: (0..192).map(|i| 1.0 + k * f64::from(i) / 192.0).collect(),
            g: vec![1.25; 192],
            b: vec![1.0 + k; 192],
        }
    }

    /// Patching must give exactly what preparing from the template gives.
    #[test]
    fn patched_configs_equal_fresh_ones() {
        let t = template();
        let mut b = BeConfigBuilder::new(t.clone()).unwrap();
        let mut s = IspSettings::from_params(&Params::default(), 0, 1.0);
        let gamma = Pwl::new(vec![(0.0, 0.0), (0.3, 0.6), (1.0, 1.0)]).unwrap();
        let mut seen = Vec::new();
        for i in 0..12u32 {
            let k = f64::from(i);
            s.wb = [1.5 + 0.01 * k, 1.0, 2.0 - 0.02 * k];
            s.digital_gain = 1.0 + 0.1 * f64::from(i % 3);
            s.ccm[1] = -0.1 * k;
            s.black_level = if i < 6 { 0.0625 } else { 0.06 };
            s.gamma = (i >= 4).then(|| gamma.clone());
            // Denoise and sharpening come on, change, and (frame 10) go off again.
            s.denoise = if (3..10).contains(&i) {
                detail(k)
            } else {
                DenoiseParams::default()
            };
            s.sharpen = (i >= 6).then_some(SharpenParams {
                threshold: 0.5 + 0.1 * k,
                strength: 1.25,
                limit: 0.6,
            });
            // Lens shading off, on, a new table, the same table, off again, on again.
            s.lens_shading = match i {
                0..=1 => None,
                2..=4 => Some(shading(0.5)),
                5..=7 => Some(shading(0.7)),
                8 => None,
                _ => Some(shading(0.7)),
            };
            seen.push(b.update(&s).unwrap());
            let mut fresh = t.clone();
            s.apply_be(&mut fresh);
            let want = fresh.prepare().unwrap();
            assert!(
                bytemuck::bytes_of(b.config()) == bytemuck::bytes_of(&*want),
                "frame {i}: {:?}",
                seen.last()
            );
        }
        assert_eq!(seen[0], BeUpdate::Rebuilt);
        assert_eq!(seen[3], BeUpdate::Rebuilt, "denoise on");
        assert_eq!(seen[4], BeUpdate::Patched, "denoise strengths");
        assert_eq!(seen[2], BeUpdate::Rebuilt, "lens shading on");
        assert_eq!(seen[8], BeUpdate::Rebuilt, "lens shading off");
        assert!(seen.contains(&BeUpdate::Patched));
        // Same settings again: nothing to do.
        assert_eq!(b.update(&s).unwrap(), BeUpdate::Unchanged);
        let c = b.counts();
        assert_eq!(c.rebuilt, 6);
        assert_eq!(c.rebuilt + c.patched + c.unchanged, 13);
    }

    fn detail(k: f64) -> DenoiseParams {
        DenoiseParams {
            noise_constant: 0.0,
            noise_slope: 5.0,
            sdn: Some(SdnParams {
                noise_constant: 0.0,
                noise_slope: 5.0 * (3.2 - 0.1 * k),
                noise_constant2: 0.0,
                noise_slope2: 16.0,
                strength: 0.82,
            }),
            cdn: Some(CdnParams {
                threshold: 1000.0 - 10.0 * k,
                strength: 0.22,
            }),
            tdn: Some(TdnParams {
                noise_constant: 0.0,
                noise_slope: 5.0,
                threshold: 0.08,
            }),
            geq: Some(GeqParams {
                offset: 239.0 * (1.0 + 0.1 * k),
                slope: 0.00766,
            }),
            dpc: 1,
        }
    }

    /// Temporal denoise: a new average at the start and after a jump of the exposure, the
    /// previous one read otherwise, scaled by the exposure ratio.
    #[test]
    fn temporal_denoise_follows_the_exposure() {
        let t = template();
        let mut b = BeConfigBuilder::new(t.clone()).unwrap();
        let mut s = IspSettings::from_params(&Params::default(), 0, 1.0);
        s.denoise = detail(0.0);
        // No buffers: no TDN whatever the settings.
        b.update_frame(&s, 1.0).unwrap();
        assert_eq!(
            b.config().config.global.bayer_enables & bayer_enable::TDN,
            0
        );
        let mut fmt = t.config().input_format;
        fmt.stride = 2560;
        b.enable_tdn(fmt);
        let mut seen = Vec::new();
        for e in [1.0, 1.0, 2.0, 9.0, 9.0, 4.5] {
            b.update_frame(&s, e).unwrap();
            let c = &b.config().config;
            let en = c.global.bayer_enables;
            assert!(en & bayer_enable::TDN != 0 && en & bayer_enable::TDN_OUTPUT != 0);
            seen.push((en & bayer_enable::TDN_INPUT != 0, c.tdn.reset, c.tdn.ratio));
        }
        let one = 1 << 14;
        assert_eq!(
            seen,
            [
                (false, 1, one),
                (true, 0, one),
                (true, 0, 2 * one),
                (false, 1, one),
                (true, 0, one),
                (true, 0, one / 2),
            ]
        );
        assert_eq!(b.config().config.tdn.threshold, 5243);
        assert_eq!(b.config().config.tdn_output_format.stride, 2560);
        // Settings without TDN switch it off; the next frame with it starts over.
        s.denoise.tdn = None;
        b.update_frame(&s, 4.5).unwrap();
        assert_eq!(
            b.config().config.global.bayer_enables & bayer_enable::TDN,
            0
        );
        s.denoise = detail(0.0);
        b.update_frame(&s, 4.5).unwrap();
        assert_eq!(b.config().config.tdn.reset, 1);
    }

    /// A crop of output 0 at full resolution: the output is the crop's size in the buffer's
    /// stride, output 1 still sees the whole frame, and a crop the back end cannot make
    /// changes nothing.
    #[test]
    fn cropped_outputs_keep_their_stride() {
        let mut b = BeConfigBuilder::new(template()).unwrap();
        let s = IspSettings::from_params(&Params::default(), 0, 1.0);
        b.update(&s).unwrap();
        let stride = b.config().config.output_format[0].image.stride;
        let crops = [
            (0, 0, 1280, 800),
            (640, 400, 320, 200),
            (2, 2, 64, 32),
            (1000, 600, 280, 200),
        ];
        for (x, y, w, h) in crops {
            let crop = BeCropConfig {
                offset_x: x,
                offset_y: y,
                width: w,
                height: h,
            };
            let t = std::time::Instant::now();
            b.set_output_crop(0, Some(crop)).unwrap();
            assert_eq!(b.update(&s).unwrap(), BeUpdate::Rebuilt);
            let took = t.elapsed();
            let c = &b.config().config;
            let out = c.output_format[0].image;
            assert_eq!(
                (out.width, out.height, out.stride),
                (w, h, stride),
                "{crop:?}"
            );
            assert_eq!(c.output_format[1].image.width, 640);
            eprintln!("crop {crop:?}: {took:?}");
        }
        b.set_output_crop(0, None).unwrap();
        b.update(&s).unwrap();
        assert_eq!(b.config().config.output_format[0].image.width, 1280);
        let wide = BeCropConfig {
            offset_x: 0,
            offset_y: 0,
            width: 2000,
            height: 800,
        };
        assert!(b.set_output_crop(0, Some(wide)).is_err());
        assert_eq!(b.update(&s).unwrap(), BeUpdate::Unchanged);
    }
}

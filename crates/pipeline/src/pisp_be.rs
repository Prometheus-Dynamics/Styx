//! The PiSP back end config for a stream of frames, rebuilt only as far as the settings
//! change.
//!
//! The algorithms move a few blocks from frame to frame (black level, white balance and
//! digital gain, CCM, now and then the gamma curve and the lens shading table); the tiles and
//! everything else depend only on the geometry. So the config and its tiles are prepared once
//! (and again only when lens shading is switched on or off, which changes the tiles), and each
//! frame patches the blocks whose register values changed. Lens shading tables are resampled
//! to the back end's grid and gamma curves converted only when they change.

use styx_algo::{LensShading, Pwl};
use styx_pisp::be::BackEnd;
use styx_pisp::uapi::{BeLscExtra, BeTilesConfig};

use crate::error::{PipelineError, Result};
use crate::isp::{IspSettings, be_lens_shading, gamma_points, level16};

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
    lens_shading: Option<LensShading>,
    counts: BeUpdateCounts,
}

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
            lens_shading: None,
            counts: BeUpdateCounts::default(),
        })
    }

    /// The config for a frame processed with `isp`.
    pub fn update(&mut self, isp: &IspSettings) -> Result<BeUpdate> {
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
        if self.fresh || lsc_on != self.lsc_on {
            let mut be = self.template.clone();
            isp.apply_be(&mut be);
            self.cfg = be.prepare().map_err(config_error)?;
            self.work = be;
            self.fresh = false;
            self.lsc_on = lsc_on;
            self.gamma.clone_from(&isp.gamma);
            self.lens_shading.clone_from(&isp.lens_shading);
            self.counts.rebuilt += 1;
            return Ok(BeUpdate::Rebuilt);
        }
        let w = &mut self.work;
        w.set_black_level(level16(isp.black_level));
        let g = isp.channel_gains();
        w.set_wb_gains(g[0], g[1], g[2]);
        w.set_ccm(isp.ccm);
        if self.gamma != isp.gamma {
            w.set_gamma_curve(&gamma_points(isp.gamma.as_ref()));
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
}

#[cfg(test)]
mod tests {
    use styx_algo::Params;
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
        assert_eq!(seen[2], BeUpdate::Rebuilt, "lens shading on");
        assert_eq!(seen[8], BeUpdate::Rebuilt, "lens shading off");
        assert!(seen.contains(&BeUpdate::Patched));
        // Same settings again: nothing to do.
        assert_eq!(b.update(&s).unwrap(), BeUpdate::Unchanged);
        let c = b.counts();
        assert_eq!(c.rebuilt, 4);
        assert_eq!(c.rebuilt + c.patched + c.unchanged, 13);
    }
}

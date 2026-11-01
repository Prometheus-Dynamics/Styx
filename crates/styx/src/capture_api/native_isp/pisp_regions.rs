//! The regions a PiSP capture makes besides its main output (`NativeIspConfig::regions`), and
//! the pyramid levels extra passes make (when the second output makes the overview, the main
//! output is a crop, or every level comes from the ISP).
//!
//! Each region is made either by the second output of the frame's own pass (the first region,
//! when nothing else takes the second output, the main output is the whole frame and the
//! region is not scaled) or by an extra back end pass over the same raw frame
//! (`PispPipeline::set_pass`), into the main output's buffers (the second output's for a
//! region in its format but not the main output's), scaled by the back end when the region
//! has a size. Either way it comes with the frame it was cut from, as a
//! `CompanionKind::Region { index }` companion whose `FrameMeta::crop` says where it is: frame,
//! regions, overview and metadata always belong to one raw frame.

use styx_capture::prelude::*;
use styx_core::prelude::{CompanionKind, FrameRect};
use styx_pipeline::device::{PispFrame, PispPipeline};
use styx_pipeline::pisp_passes::PassSpec;

use super::super::request::CaptureError;
use super::super::tunables::{MAX_NATIVE_REGIONS, NativeIspConfig, NativeRegion};
use super::crop_control::RegionCrops;
use super::err;
use super::pisp_lease::{Leaser, OutputSpec, Placed, be_crop};

/// Where a region is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The second output of the frame's own pass.
    Second,
    /// Extra pass `i`.
    Pass(usize, Shape),
}

/// What an extra pass makes of a region besides its crop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Shape {
    /// The output whose buffers it writes.
    output: usize,
    /// Its size (`None`: the crop's).
    size: Option<(u16, u16)>,
}

/// The capture's regions, as the worker keeps them. See the [module documentation](self).
#[derive(Debug)]
pub(super) struct Regions {
    places: [Option<Place>; MAX_NATIVE_REGIONS],
    /// The region each slot's back end set-up makes now (`None`: none, no companion).
    applied: [Option<FrameRect>; MAX_NATIVE_REGIONS],
    /// The pyramid levels extra passes make, and their passes.
    pyramid: Vec<(u8, usize)>,
    /// The main output's crop the pyramid passes were set up for.
    pyramid_for: Option<Option<FrameRect>>,
    frame: (u32, u32),
}

/// Extra passes `cfg` runs over every frame (region passes and the pyramid passes).
#[cfg(test)]
fn pass_count(cfg: &NativeIspConfig) -> usize {
    cfg.pass_regions().count() + cfg.pyramid_pass_levels().count()
}

impl Regions {
    /// The regions of `cfg` for a capture of `frame` size whose outputs are `specs` (the main
    /// one first); their controls in `controls` are enabled, starting from `cfg`'s rectangles.
    pub(super) fn new(
        cfg: &NativeIspConfig,
        specs: [Option<OutputSpec>; 2],
        frame: (u32, u32),
        controls: &RegionCrops,
    ) -> Result<Self, CaptureError> {
        let main = specs[0].ok_or_else(|| err("no main output"))?;
        let mut places = [None; MAX_NATIVE_REGIONS];
        let second = cfg.second_output_region();
        let mut passes = 0;
        for (k, region) in cfg.regions.iter().enumerate() {
            let Some(region) = region else { continue };
            let place = if Some(k) == second {
                Place::Second
            } else {
                let shape = pass_shape(k, *region, specs)?;
                if shape.output == 0 && region.size.is_none() && (main.width, main.height) != frame
                {
                    return Err(err(
                        "full-resolution regions from extra back end passes need the main output \
                         at the mode's size",
                    ));
                }
                passes += 1;
                Place::Pass(passes - 1, shape)
            };
            places[k] = Some(place);
            if let Some(c) = controls.slot(k) {
                c.enable(frame, region.rect);
            }
        }
        let pyramid: Vec<_> = cfg.pyramid_pass_levels().zip(passes..).collect();
        if !pyramid.is_empty() && (main.width, main.height) != frame {
            return Err(err(
                "a pyramid level from an extra back end pass needs the main output at the mode's \
                 size",
            ));
        }
        Ok(Self {
            places,
            applied: [None; MAX_NATIVE_REGIONS],
            pyramid,
            pyramid_for: None,
            frame,
        })
    }

    /// How many extra passes write into each output's buffers.
    pub(super) fn passes_per_output(&self) -> [u32; 2] {
        let mut n = [self.pyramid.len() as u32, 0];
        for place in self.places.iter().flatten() {
            if let Place::Pass(_, shape) = place {
                n[shape.output] += 1;
            }
        }
        n
    }

    /// Hands changed regions (and the main output's crop, `main_crop`, to the pyramid passes) to
    /// the back end before the next frame. A region the back end refuses reverts its control.
    pub(super) fn apply(
        &mut self,
        p: &mut PispPipeline,
        controls: &RegionCrops,
        main_crop: Option<FrameRect>,
    ) {
        for k in 0..MAX_NATIVE_REGIONS {
            let (Some(place), Some(control)) = (self.places[k], controls.slot(k)) else {
                continue;
            };
            let Some(change) = control.take() else {
                continue;
            };
            let applied = match place {
                Place::Second => p.set_output_crop(1, change.map(be_crop)),
                Place::Pass(i, shape) => p.set_pass(i, change.map(|c| shape.pass(c))),
            };
            match applied {
                Ok(()) => self.applied[k] = change,
                Err(e) => {
                    crate::trace::warn!(backend = "native", region = k + 1, error = %e, "region refused");
                    control.revert(self.applied[k]);
                }
            }
        }
        if self.pyramid_for != Some(main_crop) {
            let crop = main_crop.unwrap_or(FrameRect::new(0, 0, self.frame.0, self.frame.1));
            for &(level, i) in &self.pyramid {
                let side = |v: u32| ((v >> level) & !1).max(16).min(v & !1);
                let size = (side(crop.width) as u16, side(crop.height) as u16);
                let spec = PassSpec {
                    size: Some(size),
                    ..region_pass(crop)
                };
                if let Err(e) = p.set_pass(i, Some(spec)) {
                    crate::trace::warn!(backend = "native", level, error = %e, "pyramid pass refused");
                    let _ = p.set_pass(i, None);
                }
            }
            self.pyramid_for = Some(main_crop);
        }
    }

    /// What the second output delivers as: a region's companion (`None` when that region is
    /// not set: the output is not delivered), else `kind` as it is.
    pub(super) fn second(
        &self,
        kind: CompanionKind,
        placed: Placed,
    ) -> Option<(CompanionKind, Placed)> {
        match self.places.iter().position(|p| *p == Some(Place::Second)) {
            Some(k) => self.applied[k].map(|crop| {
                (
                    CompanionKind::Region { index: k as u8 + 1 },
                    Placed {
                        crop: Some(crop),
                        ..placed
                    },
                )
            }),
            None => Some((kind, placed)),
        }
    }

    /// The companions frame `f`'s extra passes made, leased from the outputs' buffers (laid
    /// out as `at`). On an error, the buffers not leased are handed back.
    pub(super) fn pass_companions(
        &self,
        leaser: &mut Leaser<'_>,
        f: &PispFrame,
        at: [Option<Placed>; 2],
        unleased: &mut Vec<(usize, u32)>,
    ) -> Result<Vec<(CompanionKind, FrameLease)>, String> {
        let mut out = Vec::new();
        let mut failed = None;
        for (i, pass) in f.passes.iter().enumerate() {
            let Some(pass) = pass else { continue };
            let Some(buffer) = at.get(pass.output).copied().flatten() else {
                unleased.push((pass.output, pass.index));
                continue;
            };
            let c = pass.spec.crop;
            let crop = FrameRect::new(
                u32::from(c.offset_x),
                u32::from(c.offset_y),
                u32::from(c.width),
                u32::from(c.height),
            );
            let placed = Placed {
                size: pass.spec.size.map(|(w, h)| (u32::from(w), u32::from(h))),
                crop: Some(crop),
                ..buffer
            };
            let (kind, placed) = if let Some(&(level, _)) =
                self.pyramid.iter().find(|&&(_, p)| p == i)
            {
                // Where the level sits in the whole frame at its scale, as box filters say.
                let whole = (crop.width, crop.height) == self.frame;
                let crop = (!whole).then(|| crop.scaled_down(level));
                (CompanionKind::Pyramid { level }, Placed { crop, ..placed })
            } else {
                let made_by = |p: &Option<Place>| matches!(p, Some(Place::Pass(j, _)) if *j == i);
                match self.places.iter().position(made_by) {
                    Some(k) => (CompanionKind::Region { index: k as u8 + 1 }, placed),
                    None => {
                        unleased.push((pass.output, pass.index));
                        continue;
                    }
                }
            };
            if failed.is_some() {
                unleased.push((pass.output, pass.index));
                continue;
            }
            match leaser.lease(placed, (pass.output, pass.index)) {
                Ok(lease) => out.push((kind, lease)),
                Err(e) => {
                    unleased.push((pass.output, pass.index));
                    failed = Some(e);
                }
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }
}

/// `rect` cropped at full resolution by an extra pass on output 0.
fn region_pass(rect: FrameRect) -> PassSpec {
    PassSpec {
        crop: be_crop(rect),
        output: 0,
        size: None,
        format: None,
    }
}

impl Shape {
    /// The pass making `rect` in this shape.
    fn pass(self, rect: FrameRect) -> PassSpec {
        PassSpec {
            output: self.output,
            size: self.size,
            ..region_pass(rect)
        }
    }
}

/// How an extra pass makes region `k`: in the main output's buffers, or the second output's
/// for a format only that one has (the back end lays a pass's planes out as its buffers are).
fn pass_shape(
    k: usize,
    region: NativeRegion,
    specs: [Option<OutputSpec>; 2],
) -> Result<Shape, CaptureError> {
    let main = specs[0].ok_or_else(|| err("no main output"))?;
    let code = region.format.unwrap_or(main.code);
    let output = if code == main.code {
        0
    } else if specs[1].is_some_and(|s| s.code == code) {
        1
    } else {
        return Err(err(format!(
            "region {}: an extra back end pass writes {code} only into an output in that format \
             (main {}, second {})",
            k + 1,
            main.code,
            specs[1].map_or("none".into(), |s| s.code.to_string())
        )));
    };
    let size =
        match region.size {
            Some((w, h)) => {
                let side = |v: u32| u16::try_from(v & !1).ok().filter(|&v| v >= 16);
                Some(side(w).zip(side(h)).ok_or_else(|| {
                    err(format!("region {}: size {w}x{h} is not 16..65534", k + 1))
                })?)
            }
            None => None,
        };
    Ok(Shape { output, size })
}

/// Buffers a frame's extra passes leave unleased go back to the back end through `returns`
/// (as a lease's drop does).
pub(super) fn hand_back(returns: &super::pisp_lease::Returns, unleased: &[(usize, u32)]) {
    for &b in unleased {
        let _ = returns.send(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_api::{StyxConfig, native_controls};

    fn main_spec() -> OutputSpec {
        OutputSpec {
            code: FourCc::NV12,
            width: 1280,
            height: 800,
        }
    }

    fn specs(second: Option<OutputSpec>) -> [Option<OutputSpec>; 2] {
        [Some(main_spec()), second]
    }

    fn full() -> Shape {
        Shape {
            output: 0,
            size: None,
        }
    }

    #[test]
    fn regions_go_to_the_second_output_or_extra_passes() {
        let r = |x| NativeRegion {
            rect: Some(FrameRect::new(x, 0, 64, 64)),
            ..Default::default()
        };
        let cfg = StyxConfig::default()
            .native_regions(&[r(0), r(100), r(200)])
            .backends
            .native;
        let controls = RegionCrops::default();
        let regions = Regions::new(&cfg, specs(None), (1280, 800), &controls).unwrap();
        assert_eq!(
            &regions.places[..4],
            &[
                Some(Place::Second),
                Some(Place::Pass(0, full())),
                Some(Place::Pass(1, full())),
                None
            ]
        );
        assert_eq!(pass_count(&cfg), 2);
        // Enabled from the config's rectangles; region 4 has no slot.
        assert_eq!(
            controls.slot(1).unwrap().take(),
            Some(Some(FrameRect::new(100, 0, 64, 64)))
        );
        let rect = ControlValue::Rect(ControlRect {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
        });
        assert!(controls.apply(4, &rect).is_err());
        assert!(controls.apply(3, &rect).is_ok());
        assert_eq!(
            native_controls::region_crop_index(native_controls::region_crop(3).unwrap()),
            Some(3)
        );
        // The second output delivers region 1 only while it is set.
        let placed = Placed {
            spec: main_spec(),
            stride: 1280,
            size: None,
            crop: None,
        };
        assert_eq!(regions.second(CompanionKind::Scaled, placed), None);
        // A region in another format than the main output's needs the second output.
        let rgb = NativeRegion {
            format: Some(FourCc::RG24),
            ..r(0)
        };
        let cfg = StyxConfig::default()
            .native_crop(FrameRect::new(0, 0, 256, 256))
            .native_regions(&[rgb])
            .backends
            .native;
        assert!(Regions::new(&cfg, specs(None), (1280, 800), &RegionCrops::default()).is_err());
        // ... or a second output in that format, whose buffers an extra pass writes into.
        let second = OutputSpec {
            code: FourCc::RG24,
            width: 640,
            height: 400,
        };
        let regions = Regions::new(
            &cfg,
            specs(Some(second)),
            (1280, 800),
            &RegionCrops::default(),
        )
        .unwrap();
        assert_eq!(
            regions.places[0],
            Some(Place::Pass(
                0,
                Shape {
                    output: 1,
                    ..full()
                }
            ))
        );
        assert_eq!(regions.passes_per_output(), [0, 1]);
        // A scaled region takes a pass (the second output crops at its own size), and the
        // main output need not be the whole frame for it.
        let scaled = NativeRegion {
            size: Some((129, 64)),
            ..r(0)
        };
        let cfg = StyxConfig::default()
            .native_regions(&[scaled])
            .backends
            .native;
        assert_eq!(cfg.second_output_region(), None);
        let small = OutputSpec {
            width: 640,
            height: 400,
            ..main_spec()
        };
        let regions = Regions::new(
            &cfg,
            [Some(small), None],
            (1280, 800),
            &RegionCrops::default(),
        )
        .unwrap();
        let shape = Shape {
            size: Some((128, 64)),
            ..full()
        };
        assert_eq!(regions.places[0], Some(Place::Pass(0, shape)));
        let pass = shape.pass(FrameRect::new(0, 0, 256, 128));
        assert_eq!(
            (pass.output, pass.size, pass.crop.width),
            (0, Some((128, 64)), 256)
        );
        let tiny = NativeRegion {
            size: Some((8, 8)),
            ..r(0)
        };
        let cfg = StyxConfig::default()
            .native_regions(&[tiny])
            .backends
            .native;
        assert!(Regions::new(&cfg, specs(None), (1280, 800), &RegionCrops::default()).is_err());
        // An overview with a pyramid level: the level from a pass after the regions'.
        let cfg = StyxConfig::default()
            .native_overview(320, 200)
            .native_pyramid_level(1)
            .native_regions(&[r(0)])
            .backends
            .native;
        let regions =
            Regions::new(&cfg, specs(None), (1280, 800), &RegionCrops::default()).unwrap();
        assert_eq!(regions.places[0], Some(Place::Pass(0, full())));
        assert_eq!(regions.pyramid, [(1, 1)]);
        assert_eq!(pass_count(&cfg), 2);
    }

    #[test]
    fn every_pyramid_level_from_the_isp() {
        // The second output makes level 1, a pass each the deeper ones.
        let cfg = StyxConfig::default()
            .native_pyramid_level(3)
            .native_pyramid_all_levels(true)
            .backends
            .native;
        assert_eq!(cfg.pyramid_output_level(), 1);
        let regions =
            Regions::new(&cfg, specs(None), (1280, 800), &RegionCrops::default()).unwrap();
        assert_eq!(regions.pyramid, [(2, 0), (3, 1)]);
        assert_eq!(pass_count(&cfg), 2);
        // With a crop, every level from a pass, after the regions'.
        let region = NativeRegion {
            rect: Some(FrameRect::new(0, 0, 64, 64)),
            ..Default::default()
        };
        let cfg = StyxConfig::default()
            .native_crop(FrameRect::new(0, 0, 640, 400))
            .native_pyramid_level(2)
            .native_pyramid_all_levels(true)
            .native_regions(&[region])
            .backends
            .native;
        assert_eq!(cfg.pyramid_output_level(), 0);
        let regions =
            Regions::new(&cfg, specs(None), (1280, 800), &RegionCrops::default()).unwrap();
        assert_eq!(regions.pyramid, [(1, 1), (2, 2)]);
        assert_eq!(pass_count(&cfg), 3);
        // Only the deepest level otherwise, from the second output.
        let cfg = StyxConfig::default()
            .native_pyramid_level(3)
            .backends
            .native;
        assert_eq!(cfg.pyramid_output_level(), 3);
        assert!(!cfg.pyramid_pass());
        assert_eq!(pass_count(&cfg), 0);
    }
}

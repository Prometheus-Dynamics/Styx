//! The regions a PiSP capture makes besides its main output (`NativeIspConfig::regions`), and
//! a pyramid level from an extra pass when the second output makes the overview.
//!
//! Each region is made either by the second output of the frame's own pass (the first region,
//! when nothing else takes the second output and the main output is the whole frame) or by an
//! extra back end pass over the same raw frame (`PispPipeline::set_pass`), into the main
//! output's buffers. Either way it comes with the frame it was cut from, as a
//! `CompanionKind::Region { index }` companion whose `FrameMeta::crop` says where it is: frame,
//! regions, overview and metadata always belong to one raw frame.

use std::sync::mpsc;

use styx_capture::prelude::*;
use styx_core::prelude::{CompanionKind, FrameRect};
use styx_pipeline::device::{PispFrame, PispPipeline};
use styx_pipeline::pisp_passes::PassSpec;

use super::super::request::CaptureError;
use super::super::tunables::{MAX_NATIVE_REGIONS, NativeIspConfig};
use super::crop_control::RegionCrops;
use super::err;
use super::pisp_lease::{Leaser, OutputSpec, Placed, be_crop};

/// Where a region is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The second output of the frame's own pass.
    Second,
    /// Extra pass `i`.
    Pass(usize),
}

/// The capture's regions, as the worker keeps them. See the [module documentation](self).
#[derive(Debug)]
pub(super) struct Regions {
    places: [Option<Place>; MAX_NATIVE_REGIONS],
    /// The region each slot's back end set-up makes now (`None`: none, no companion).
    applied: [Option<FrameRect>; MAX_NATIVE_REGIONS],
    /// The pyramid level an extra pass makes, and that pass.
    pyramid: Option<(u8, usize)>,
    /// The main output's crop the pyramid pass was set up for.
    pyramid_for: Option<Option<FrameRect>>,
    frame: (u32, u32),
}

/// Extra passes `cfg` runs over every frame (region passes and the pyramid pass).
pub(super) fn pass_count(cfg: &NativeIspConfig) -> usize {
    cfg.pass_regions().count() + usize::from(cfg.pyramid_pass())
}

impl Regions {
    /// The regions of `cfg` for a capture of `frame` size whose main output is `main`; their
    /// controls in `controls` are enabled, starting from `cfg`'s rectangles.
    pub(super) fn new(
        cfg: &NativeIspConfig,
        main: OutputSpec,
        frame: (u32, u32),
        controls: &RegionCrops,
    ) -> Result<Self, CaptureError> {
        let mut places = [None; MAX_NATIVE_REGIONS];
        let second = cfg.second_output_region();
        let mut passes = 0;
        for (k, region) in cfg.regions.iter().enumerate() {
            let Some(region) = region else { continue };
            let place = if Some(k) == second {
                Place::Second
            } else {
                if (main.width, main.height) != frame {
                    return Err(err(
                        "regions from extra back end passes need the main output at the mode's size",
                    ));
                }
                if region.format.is_some_and(|f| f != main.code) {
                    return Err(err(format!(
                        "region {}: an extra back end pass makes it in the main output's format \
                         ({}); only the second output's region can differ",
                        k + 1,
                        main.code
                    )));
                }
                passes += 1;
                Place::Pass(passes - 1)
            };
            places[k] = Some(place);
            if let Some(c) = controls.slot(k) {
                c.enable(frame, region.rect);
            }
        }
        let pyramid = cfg
            .pyramid_pass()
            .then(|| (cfg.pyramid_level.clamp(1, 3), passes));
        if pyramid.is_some() && (main.width, main.height) != frame {
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

    /// Hands changed regions (and the main output's crop, `main_crop`, to the pyramid pass) to
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
                Place::Pass(i) => p.set_pass(i, change.map(region_pass)),
            };
            match applied {
                Ok(()) => self.applied[k] = change,
                Err(e) => {
                    tracing::warn!(backend = "native", region = k + 1, error = %e, "region refused");
                    control.revert(self.applied[k]);
                }
            }
        }
        if let Some((level, i)) = self.pyramid
            && self.pyramid_for != Some(main_crop)
        {
            let crop = main_crop.unwrap_or(FrameRect::new(0, 0, self.frame.0, self.frame.1));
            let side = |v: u32| ((v >> level) & !1).max(16).min(v & !1);
            let size = (side(crop.width) as u16, side(crop.height) as u16);
            let spec = PassSpec {
                size: Some(size),
                ..region_pass(crop)
            };
            match p.set_pass(i, Some(spec)) {
                Ok(()) => self.pyramid_for = Some(main_crop),
                Err(e) => {
                    tracing::warn!(backend = "native", error = %e, "pyramid pass refused");
                    self.pyramid_for = Some(main_crop);
                    let _ = p.set_pass(i, None);
                }
            }
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

    /// The companions frame `f`'s extra passes made, leased from the main output's buffers
    /// (laid out as `main`). On an error, the buffers not leased are handed back.
    pub(super) fn pass_companions(
        &self,
        leaser: &mut Leaser<'_>,
        f: &PispFrame,
        main: Placed,
        unleased: &mut Vec<(usize, u32)>,
    ) -> Result<Vec<(CompanionKind, FrameLease)>, String> {
        let mut out = Vec::new();
        let mut failed = None;
        for (i, pass) in f.passes.iter().enumerate() {
            let Some(pass) = pass else { continue };
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
                ..main
            };
            let (kind, placed) = if let Some((level, _)) = self.pyramid.filter(|&(_, p)| p == i) {
                // Where the level sits in the whole frame at its scale, as box filters say.
                let whole = (crop.width, crop.height) == self.frame;
                let crop = (!whole).then(|| crop.scaled_down(level));
                (CompanionKind::Pyramid { level }, Placed { crop, ..placed })
            } else {
                match self.places.iter().position(|p| *p == Some(Place::Pass(i))) {
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

/// Buffers a frame's extra passes leave unleased go back to the back end through `returns`
/// (as a lease's drop does).
pub(super) fn hand_back(returns: &mpsc::Sender<(usize, u32)>, unleased: &[(usize, u32)]) {
    for &b in unleased {
        let _ = returns.send(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_api::tunables::NativeRegion;
    use crate::capture_api::{StyxConfig, native_controls};

    fn main_spec() -> OutputSpec {
        OutputSpec {
            code: FourCc::NV12,
            width: 1280,
            height: 800,
        }
    }

    #[test]
    fn regions_go_to_the_second_output_or_extra_passes() {
        let r = |x| NativeRegion {
            rect: Some(FrameRect::new(x, 0, 64, 64)),
            format: None,
        };
        let cfg = StyxConfig::default()
            .native_regions(&[r(0), r(100), r(200)])
            .backends
            .native;
        let controls = RegionCrops::default();
        let regions = Regions::new(&cfg, main_spec(), (1280, 800), &controls).unwrap();
        assert_eq!(
            &regions.places[..4],
            &[
                Some(Place::Second),
                Some(Place::Pass(0)),
                Some(Place::Pass(1)),
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
        assert!(Regions::new(&cfg, main_spec(), (1280, 800), &RegionCrops::default()).is_err());
        // An overview with a pyramid level: the level from a pass after the regions'.
        let cfg = StyxConfig::default()
            .native_overview(320, 200)
            .native_pyramid_level(1)
            .native_regions(&[r(0)])
            .backends
            .native;
        let regions =
            Regions::new(&cfg, main_spec(), (1280, 800), &RegionCrops::default()).unwrap();
        assert_eq!(regions.places[0], Some(Place::Pass(0)));
        assert_eq!(regions.pyramid, Some((1, 1)));
        assert_eq!(pass_count(&cfg), 2);
    }
}

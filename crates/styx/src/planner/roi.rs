//! The live regions of interest of a running plan ([`RoiHandle`]): kept for the frames the
//! plan crops itself (views), and handed to the ISP for the ones it crops (`OUTPUT_CROP` for
//! the main output's, `region_crop(index)` for the others).

use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use styx_core::prelude::*;

use super::region::IspPlace;
use super::request::MAX_REGIONS;
use crate::capture_api::{CaptureHandle, ControlPlane};

/// Live region-of-interest control for a running plan. Cloneable; changes apply to the next
/// frame (on an ISP crop, [`RoiCrop::Isp`](super::RoiCrop), the next frame the ISP processes).
/// Coordinates are full-frame pixels. Region 0 is the frame itself ([`FrameRequest::roi`]),
/// regions 1, 2, ... its `CompanionKind::Region` companions ([`FrameRequest::regions`]).
///
/// [`FrameRequest::roi`]: super::FrameRequest::roi
/// [`FrameRequest::regions`]: super::FrameRequest::regions
#[derive(Clone, Default)]
pub struct RoiHandle {
    rects: Arc<Mutex<Vec<Option<FrameRect>>>>,
    /// The capture whose ISP crops regions, and where it makes each.
    isp: Arc<OnceLock<(ControlPlane, Vec<Option<IspPlace>>)>>,
}

impl RoiHandle {
    /// Region 0 (`None`: the whole frame).
    pub fn set(&self, roi: Option<FrameRect>) {
        self.set_region(0, roi);
    }

    /// Region 0.
    pub fn get(&self) -> Option<FrameRect> {
        self.region(0)
    }

    /// Region `index` (`None`: none; for region 0, the whole frame). Only regions the request
    /// asked for are delivered ([`FrameRequest::regions`](super::FrameRequest::regions)).
    pub fn set_region(&self, index: usize, rect: Option<FrameRect>) {
        if index >= MAX_REGIONS {
            return;
        }
        {
            let mut rects = self.rects.lock();
            if rects.len() <= index {
                rects.resize(index + 1, None);
            }
            rects[index] = rect;
        }
        if let Some((plane, places)) = self.isp.get()
            && let Some(Some(place)) = places.get(index)
        {
            set_isp_crop(plane, *place, rect);
        }
    }

    /// All regions at once: `regions[0]` is region 0 (the frame), the others its companions;
    /// regions beyond the list are cleared.
    pub fn set_regions(&self, regions: &[FrameRect]) {
        let n = self.rects.lock().len().max(regions.len()).min(MAX_REGIONS);
        for i in 0..n {
            self.set_region(i, regions.get(i).copied());
        }
    }

    /// Region `index` as set.
    pub fn region(&self, index: usize) -> Option<FrameRect> {
        self.rects.lock().get(index).copied().flatten()
    }

    /// The regions as set, region 0 first (`None` where one is not set).
    pub fn regions(&self) -> Vec<Option<FrameRect>> {
        self.rects.lock().clone()
    }

    /// From now on the ISP of `capture` crops the regions `places` names, starting from the
    /// regions as set.
    pub(crate) fn crop_in_isp(&self, capture: &CaptureHandle, places: Vec<Option<IspPlace>>) {
        if self.isp.set((capture.control.clone(), places)).is_err() {
            return;
        }
        let rects = self.regions();
        if let Some((plane, places)) = self.isp.get() {
            for (place, rect) in places
                .iter()
                .zip(rects.iter().chain(std::iter::repeat(&None)))
            {
                if let Some(place) = place {
                    set_isp_crop(plane, *place, *rect);
                }
            }
        }
    }
}

/// `rect` as the ISP's crop at `place` (`None`: the whole frame for the main output, no region
/// otherwise). Only native cameras crop in their ISP.
#[cfg(feature = "native")]
fn set_isp_crop(plane: &ControlPlane, place: IspPlace, rect: Option<FrameRect>) {
    use crate::capture_api::{apply_control_to_plane, native_controls};
    let r = rect.unwrap_or(FrameRect::new(0, 0, 0, 0));
    let value = ControlValue::Rect(ControlRect {
        x: i32::try_from(r.x).unwrap_or(i32::MAX),
        y: i32::try_from(r.y).unwrap_or(i32::MAX),
        width: r.width,
        height: r.height,
    });
    let id = match place {
        IspPlace::Main => Some(native_controls::OUTPUT_CROP),
        IspPlace::Slot(k) => native_controls::region_crop(k + 1),
    };
    let Some(id) = id else { return };
    if let Err(err) = apply_control_to_plane(plane, id, value) {
        tracing::warn!(error = %err, "region of interest not applied by the ISP");
    }
}

#[cfg(not(feature = "native"))]
fn set_isp_crop(_: &ControlPlane, _: IspPlace, _: Option<FrameRect>) {}

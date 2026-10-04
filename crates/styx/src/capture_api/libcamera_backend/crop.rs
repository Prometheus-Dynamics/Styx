//! Regions of interest cropped by a Raspberry Pi ISP through libcamera (`rpi::ScalerCrops`, one
//! crop per output): the main output shows the region, scaled to its stream's size, the second
//! (an overview) the whole field of view. `OUTPUT_CROP` (frame pixels of the mode) is turned into
//! sensor coordinates here; each frame says which region it shows (`FrameMeta::crop`), from its
//! request's metadata. A new region shows 2-3 frames after it is set (the requests already
//! queued carry the old one).

use std::collections::HashMap;

use styx_core::controls::{ControlId, ControlRect, ControlValue};
use styx_core::prelude::FrameRect;

/// `rpi::ScalerCrops`: one rectangle per output, in sensor coordinates.
pub(super) const SCALER_CROPS: ControlId = ControlId(20003);
/// Styx's crop of the main output, in the mode's frame pixels.
pub(crate) use crate::capture_api::OUTPUT_CROP;

/// The crop of a capture whose ISP crops.
#[derive(Debug)]
pub(super) struct Crop {
    /// The mode's frame: the whole field of view.
    frame: (u32, u32),
    /// The sensor area of the whole field of view (`ScalerCropMaximum`, or the first frames'
    /// crops before any is set).
    area: Option<ControlRect>,
    /// ISP outputs: one crop each (the second keeps the whole field of view).
    outputs: usize,
    /// The region as set (in frame pixels), applied once the area is known.
    wanted: Option<FrameRect>,
    pending: bool,
}

impl Crop {
    /// A capture of `frame`-sized frames on `outputs` ISP outputs, starting with `initial`.
    pub(super) fn new(frame: (u32, u32), outputs: usize, initial: Option<FrameRect>) -> Self {
        Self {
            frame,
            area: None,
            outputs: outputs.max(1),
            wanted: initial,
            pending: initial.is_some(),
        }
    }

    /// The crop of a configured camera whose mode's frames are `full` (the whole field of
    /// view), with or without a second output.
    pub(super) fn start(
        cam: &libcamera::camera::Camera<'_>,
        full: styx_core::prelude::Resolution,
        second: bool,
        initial: Option<FrameRect>,
    ) -> Self {
        let frame = (full.width.get(), full.height.get());
        let mut crop = Self::new(frame, if second { 2 } else { 1 }, initial);
        if let Some(area) = area_from_properties(cam) {
            crop.set_area(area);
        }
        crop
    }

    /// The sensor area of the whole field of view, when libcamera reports it.
    pub(super) fn set_area(&mut self, area: ControlRect) {
        if area.width > 0 && area.height > 0 {
            self.area = Some(area);
        }
    }

    /// `updates` with `OUTPUT_CROP` taken out (remembered, sent as `ScalerCrops` once the
    /// sensor area is known).
    pub(super) fn take_updates(
        &mut self,
        mut updates: HashMap<ControlId, Option<ControlValue>>,
    ) -> HashMap<ControlId, Option<ControlValue>> {
        if let Some(value) = updates.remove(&OUTPUT_CROP) {
            self.wanted = match value {
                Some(ControlValue::Rect(r))
                    if r.width > 0 && r.height > 0 && r.x >= 0 && r.y >= 0 =>
                {
                    FrameRect::new(r.x as u32, r.y as u32, r.width, r.height)
                        .clipped_to(self.frame.0, self.frame.1)
                }
                _ => None,
            };
            self.pending = true;
        }
        updates
    }

    /// After a request's metadata (`readback`): learns the sensor area from the first frames,
    /// puts a waiting region into `control_state` (sent with every request), and returns the
    /// region this request's frame shows (`None`: the whole frame).
    pub(super) fn observe(
        &mut self,
        readback: &HashMap<ControlId, ControlValue>,
        control_state: &mut HashMap<ControlId, ControlValue>,
    ) -> Option<FrameRect> {
        let shown = match readback.get(&SCALER_CROPS) {
            Some(ControlValue::Rects(r)) => r.first().copied(),
            Some(ControlValue::Rect(r)) => Some(*r),
            _ => None,
        };
        if self.area.is_none()
            && !control_state.contains_key(&SCALER_CROPS)
            && let Some(r) = shown
        {
            self.set_area(r);
        }
        if self.pending
            && let Some(area) = self.area
        {
            let whole = to_sensor(
                FrameRect::new(0, 0, self.frame.0, self.frame.1),
                self.frame,
                area,
            );
            let main = self
                .wanted
                .map_or(whole, |r| to_sensor(r, self.frame, area));
            let mut rects = vec![main];
            rects.resize(self.outputs, whole);
            control_state.insert(SCALER_CROPS, ControlValue::Rects(rects));
            self.pending = false;
        }
        let area = self.area?;
        let r = to_frame(shown?, self.frame, area);
        (r != FrameRect::new(0, 0, self.frame.0, self.frame.1)).then_some(r)
    }

    /// The region as set (`OUTPUT_CROP`'s value: zero size for the whole frame).
    pub(super) fn read(&self) -> ControlValue {
        let r = self.wanted.unwrap_or(FrameRect::new(0, 0, 0, 0));
        ControlValue::Rect(ControlRect {
            x: r.x as i32,
            y: r.y as i32,
            width: r.width,
            height: r.height,
        })
    }
}

/// The sensor area of the configured mode's whole field of view (`ScalerCropMaximum`, set when
/// the camera is configured).
pub(super) fn area_from_properties(cam: &libcamera::camera::Camera<'_>) -> Option<ControlRect> {
    use libcamera::control_value::ControlValue as Lc;
    cam.properties().into_iter().find_map(|(id, val)| {
        let named = libcamera::properties::PropertyId::try_from(id)
            .is_ok_and(|p| p.name() == "ScalerCropMaximum");
        match (named, val) {
            (true, Lc::Rectangle(v)) => v.first().map(|r| ControlRect {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            }),
            _ => None,
        }
    })
}

/// `r` (frame pixels of a `frame`-sized frame) in the sensor coordinates of `area`.
fn to_sensor(r: FrameRect, frame: (u32, u32), area: ControlRect) -> ControlRect {
    let sx = |v: u32| (u64::from(v) * u64::from(area.width) / u64::from(frame.0.max(1))) as u32;
    let sy = |v: u32| (u64::from(v) * u64::from(area.height) / u64::from(frame.1.max(1))) as u32;
    ControlRect {
        x: area.x + sx(r.x) as i32,
        y: area.y + sy(r.y) as i32,
        width: sx(r.width).max(1),
        height: sy(r.height).max(1),
    }
}

/// `r` (sensor coordinates of `area`) in frame pixels, rounded to the nearest pixel.
fn to_frame(r: ControlRect, frame: (u32, u32), area: ControlRect) -> FrameRect {
    let fx = |v: i64| {
        ((v * i64::from(frame.0) + i64::from(area.width) / 2) / i64::from(area.width.max(1))).max(0)
            as u32
    };
    let fy = |v: i64| {
        ((v * i64::from(frame.1) + i64::from(area.height) / 2) / i64::from(area.height.max(1)))
            .max(0) as u32
    };
    let (x, y) = (fx(i64::from(r.x - area.x)), fy(i64::from(r.y - area.y)));
    FrameRect::new(x, y, fx(i64::from(r.width)), fy(i64::from(r.height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> ControlRect {
        ControlRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn regions_go_to_the_sensor_once_its_area_is_known() {
        let mut crop = Crop::new((1280, 720), 2, Some(FrameRect::new(480, 260, 320, 200)));
        let mut state = HashMap::new();
        // The first frame's metadata: the whole field of view, a 1280x720 window of the sensor.
        let whole = rect(0, 40, 1280, 720);
        let readback = HashMap::from([(SCALER_CROPS, ControlValue::Rects(vec![whole, whole]))]);
        assert_eq!(crop.observe(&readback, &mut state), None);
        assert_eq!(
            state.get(&SCALER_CROPS),
            Some(&ControlValue::Rects(vec![rect(480, 300, 320, 200), whole]))
        );
        // The frame showing it says so.
        let shown = HashMap::from([(
            SCALER_CROPS,
            ControlValue::Rects(vec![rect(480, 300, 320, 200), whole]),
        )]);
        assert_eq!(
            crop.observe(&shown, &mut state),
            Some(FrameRect::new(480, 260, 320, 200))
        );
        // Moved with OUTPUT_CROP, and back to the whole frame.
        let moved = ControlValue::Rect(rect(0, 0, 256, 256));
        let rest = crop.take_updates(HashMap::from([
            (OUTPUT_CROP, Some(moved.clone())),
            (ControlId(7), None),
        ]));
        assert_eq!(rest.len(), 1);
        assert_eq!(crop.read(), moved);
        crop.observe(&shown, &mut state);
        assert_eq!(
            state.get(&SCALER_CROPS),
            Some(&ControlValue::Rects(vec![rect(0, 40, 256, 256), whole]))
        );
        crop.take_updates(HashMap::from([(OUTPUT_CROP, None)]));
        crop.observe(&shown, &mut state);
        assert_eq!(
            state.get(&SCALER_CROPS),
            Some(&ControlValue::Rects(vec![whole, whole]))
        );
    }
}

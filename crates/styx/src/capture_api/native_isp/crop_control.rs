//! The main output's crop of a processed native capture (`OUTPUT_CROP`): the PiSP's back end
//! delivers that region of the frame at full resolution, and frames say where it is
//! (`FrameMeta::crop`). Set by the application, or by the planner for a request's region of
//! interest; the worker hands a changed crop to the back end before its next frame.

use parking_lot::Mutex;
use styx_capture::prelude::*;
use styx_core::prelude::FrameRect;
use styx_native::CameraInfo;

use super::super::native_backend::controls as ids;
use super::super::request::CaptureError;

/// Smallest crop the back end makes (its smallest tile).
const MIN_SIDE: u32 = 16;

/// The crop as set, and a change the worker has not taken yet.
#[derive(Debug, Default)]
pub(crate) struct CropControl {
    /// The frame crops lie in; `None` while this capture cannot crop (the software ISP, or a
    /// main output scaled to another size).
    frame: Mutex<Option<(u32, u32)>>,
    current: Mutex<Option<FrameRect>>,
    pending: Mutex<Option<Option<FrameRect>>>,
}

impl CropControl {
    /// Crops within a `frame`-sized frame are possible from now on, starting with `initial`.
    pub(crate) fn enable(&self, frame: (u32, u32), initial: Option<FrameRect>) {
        *self.frame.lock() = Some(frame);
        if let Some(rect) = initial {
            self.set(fit(rect, frame));
        }
    }

    /// Applies an `OUTPUT_CROP` value: a rectangle of the frame (rounded out to even pixels,
    /// at least 16x16, clipped to the frame), or one of zero size for the whole frame.
    pub(crate) fn apply(&self, value: &ControlValue) -> Result<(), CaptureError> {
        let Some(frame) = *self.frame.lock() else {
            return Err(CaptureError::control_apply(
                "output crop: only the PiSP crops, with its main output at the mode's size",
            ));
        };
        let rect = match value {
            ControlValue::None => None,
            ControlValue::Rect(r) if r.width == 0 || r.height == 0 => None,
            ControlValue::Rect(r) if r.x >= 0 && r.y >= 0 => {
                Some(FrameRect::new(r.x as u32, r.y as u32, r.width, r.height))
            }
            _ => {
                return Err(CaptureError::control_apply(
                    "output crop: a rectangle in the frame (zero size: the whole frame)",
                ));
            }
        };
        match rect {
            Some(rect) => match fit(rect, frame) {
                Some(rect) => self.set(Some(rect)),
                None => {
                    return Err(CaptureError::control_apply(
                        "output crop: the rectangle is outside the frame",
                    ));
                }
            },
            None => self.set(None),
        }
        Ok(())
    }

    fn set(&self, rect: Option<FrameRect>) {
        let mut current = self.current.lock();
        if *current != rect {
            *current = rect;
            *self.pending.lock() = Some(rect);
        }
    }

    /// The crop as set (a zero-size rectangle for the whole frame).
    pub(crate) fn read(&self) -> ControlValue {
        let r = self.current.lock().unwrap_or(FrameRect::new(0, 0, 0, 0));
        ControlValue::Rect(ControlRect {
            x: r.x as i32,
            y: r.y as i32,
            width: r.width,
            height: r.height,
        })
    }

    /// A crop set since the last call, for the worker (`Some(None)`: back to the whole frame).
    pub(crate) fn take(&self) -> Option<Option<FrameRect>> {
        self.pending.lock().take()
    }

    /// The crop is refused by the back end: back to `rect` (what the frames have).
    pub(crate) fn revert(&self, rect: Option<FrameRect>) {
        *self.current.lock() = rect;
    }
}

/// The `OUTPUT_CROP` control of a camera whose processed modes run on the PiSP: rectangles up
/// to its largest mode.
pub(crate) fn meta(info: &CameraInfo) -> Option<ControlMeta> {
    let (width, height) = info.modes.iter().map(|m| (m.width, m.height)).max()?;
    let rect = |width, height| {
        ControlValue::Rect(ControlRect {
            x: 0,
            y: 0,
            width,
            height,
        })
    };
    Some(ControlMeta {
        id: ids::OUTPUT_CROP,
        name: "output_crop".into(),
        kind: ControlKind::Rectangle,
        access: Access::ReadWrite,
        min: rect(0, 0),
        max: rect(width, height),
        default: rect(0, 0),
        step: Some(rect(2, 2)),
        menu: None,
        metadata: ControlMetadata::default(),
    })
}

/// `rect` as the back end can crop it from a `frame`-sized frame: clipped to the frame,
/// rounded out to even pixels, at least 16x16 (grown towards the frame's middle). `None` when
/// it does not overlap the frame.
pub(crate) fn fit(rect: FrameRect, frame: (u32, u32)) -> Option<FrameRect> {
    let rect = rect.clipped_to(frame.0, frame.1)?;
    let axis = |start: u32, len: u32, size: u32| {
        let size = size & !1;
        let mut lo = start & !1;
        let mut hi = (start + len).next_multiple_of(2).min(size);
        let min = MIN_SIDE.min(size);
        if hi - lo < min {
            hi = (lo + min).min(size);
            lo = hi - min;
        }
        (lo, hi - lo)
    };
    let (x, width) = axis(rect.x, rect.width, frame.0);
    let (y, height) = axis(rect.y, rect.height, frame.1);
    Some(FrameRect::new(x, y, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_round_out_to_even_pixels_inside_the_frame() {
        let frame = (1280, 800);
        let fit = |x, y, w, h| fit(FrameRect::new(x, y, w, h), frame);
        assert_eq!(fit(0, 0, 1280, 800), Some(FrameRect::new(0, 0, 1280, 800)));
        assert_eq!(fit(101, 51, 99, 49), Some(FrameRect::new(100, 50, 100, 50)));
        // Too small: 16x16, kept inside the frame.
        assert_eq!(
            fit(1275, 795, 4, 4),
            Some(FrameRect::new(1264, 784, 16, 16))
        );
        assert_eq!(
            fit(1200, 700, 200, 200),
            Some(FrameRect::new(1200, 700, 80, 100))
        );
        assert_eq!(fit(2000, 0, 10, 10), None);
    }

    #[test]
    fn the_control_needs_a_capture_that_crops() {
        let c = CropControl::default();
        let rect = |x, y, w, h| {
            ControlValue::Rect(ControlRect {
                x,
                y,
                width: w,
                height: h,
            })
        };
        assert!(c.apply(&rect(0, 0, 64, 64)).is_err());
        c.enable((1280, 800), Some(FrameRect::new(10, 10, 100, 100)));
        assert_eq!(c.take(), Some(Some(FrameRect::new(10, 10, 100, 100))));
        c.apply(&rect(11, 0, 64, 64)).unwrap();
        assert_eq!(c.read(), rect(10, 0, 66, 64));
        assert_eq!(c.take(), Some(Some(FrameRect::new(10, 0, 66, 64))));
        // The same crop again is no change.
        c.apply(&rect(11, 0, 64, 64)).unwrap();
        assert_eq!(c.take(), None);
        c.apply(&rect(0, 0, 0, 0)).unwrap();
        assert_eq!(c.take(), Some(None));
        assert!(c.apply(&rect(-4, 0, 64, 64)).is_err());
        assert!(c.apply(&ControlValue::Int(3)).is_err());
    }
}

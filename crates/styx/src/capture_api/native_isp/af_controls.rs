//! Autofocus controls of a processed native capture whose camera has a focus lens: mode,
//! trigger, range, speed, metering and windows, the lens position in dioptres, and the state
//! AF reports. Values follow libcamera's `AfMode`, `AfTrigger`, `AfState`, `AfRange`,
//! `AfSpeed`, `AfMetering`, `AfWindows` and `LensPosition` (dioptres).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use parking_lot::Mutex;
use styx_capture::prelude::{
    Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlRect, ControlValue,
};
use styx_pipeline::styx_algo::{AfMode, AfRange, AfSpeed, AfState, AfWindow, Controls, Params};

use super::super::native_backend::controls as ids;
use super::super::request::CaptureError;

/// What AF reported for the latest frame, and the windows as set.
#[derive(Debug)]
pub(crate) struct AfReport {
    state: AtomicI32,
    /// Lens position in dioptres (`f32` bits), NaN before AF has one.
    lens_position: AtomicU32,
    /// The processed output's size, for windows in pixels.
    output: Mutex<(u32, u32)>,
    windows: Mutex<Vec<ControlRect>>,
    metering_windows: Mutex<bool>,
    /// The camera has a focus lens (else the AF controls are not supported).
    lens: AtomicBool,
}

impl Default for AfReport {
    fn default() -> Self {
        Self {
            state: AtomicI32::new(0),
            lens_position: AtomicU32::new(f32::NAN.to_bits()),
            output: Mutex::new((0, 0)),
            windows: Mutex::new(Vec::new()),
            metering_windows: Mutex::new(false),
            lens: AtomicBool::new(false),
        }
    }
}

impl AfReport {
    /// The output size AF windows are given in.
    pub(crate) fn set_output(&self, width: u32, height: u32) {
        *self.output.lock() = (width, height);
    }

    /// Whether the camera has a focus lens.
    pub(crate) fn set_lens(&self, lens: bool) {
        self.lens.store(lens, Ordering::Release);
    }
}

/// Whether `id` is one of the AF controls.
pub(crate) fn is_af(id: ControlId) -> bool {
    [
        ids::AF_MODE,
        ids::AF_TRIGGER,
        ids::AF_STATE,
        ids::LENS_POSITION,
        ids::AF_WINDOWS,
        ids::AF_METERING,
        ids::AF_RANGE,
        ids::AF_SPEED,
    ]
    .contains(&id)
}

fn int(value: &ControlValue) -> Option<i64> {
    match value {
        ControlValue::Int(v) => Some(i64::from(*v)),
        ControlValue::Uint(v) => Some(i64::from(*v)),
        ControlValue::Bool(v) => Some(i64::from(*v)),
        _ => None,
    }
}

fn windows_to(r: &AfReport, rects: &[ControlRect]) -> Vec<AfWindow> {
    let (w, h) = *r.output.lock();
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let (w, h) = (f64::from(w), f64::from(h));
    rects
        .iter()
        .map(|c| {
            AfWindow::new(
                f64::from(c.x) / w,
                f64::from(c.y) / h,
                f64::from(c.width) / w,
                f64::from(c.height) / h,
            )
        })
        .collect()
}

/// Applies an AF control to `c` (`None` when `id` is not one).
pub(crate) fn apply(
    r: &AfReport,
    id: ControlId,
    value: &ControlValue,
    c: &mut Controls,
) -> Option<Result<(), CaptureError>> {
    let bad = |m: &str| Some(Err(CaptureError::control_apply(m.to_owned())));
    if !is_af(id) {
        return None;
    }
    if !r.lens.load(Ordering::Acquire) {
        return Some(Err(CaptureError::ControlUnsupported));
    }
    match id {
        ids::AF_MODE => {
            c.af_mode = match int(value) {
                Some(0) => AfMode::Manual,
                Some(1) => AfMode::Auto,
                Some(2) => AfMode::Continuous,
                _ => return bad("AF mode: 0 manual, 1 auto, 2 continuous"),
            }
        }
        ids::AF_TRIGGER => match int(value) {
            Some(0) => c.af_trigger = c.af_trigger.wrapping_add(1),
            Some(1) => c.af_cancel = c.af_cancel.wrapping_add(1),
            _ => return bad("AF trigger: 0 start, 1 cancel"),
        },
        ids::AF_RANGE => {
            c.af_range = match int(value) {
                Some(0) => AfRange::Normal,
                Some(1) => AfRange::Macro,
                Some(2) => AfRange::Full,
                _ => return bad("AF range: 0 normal, 1 macro, 2 full"),
            }
        }
        ids::AF_SPEED => {
            c.af_speed = match int(value) {
                Some(0) => AfSpeed::Normal,
                Some(1) => AfSpeed::Fast,
                _ => return bad("AF speed: 0 normal, 1 fast"),
            }
        }
        ids::LENS_POSITION => {
            let d = match value {
                ControlValue::Float(v) => f64::from(*v),
                v => match int(v) {
                    Some(i) => i as f64,
                    None => return bad("lens position: dioptres"),
                },
            };
            if !(d.is_finite() && d >= 0.0) {
                return bad("lens position: dioptres, 0 (infinity) or more");
            }
            c.lens_position = Some(d);
        }
        ids::AF_METERING => {
            let windows = match int(value) {
                Some(0) => false,
                Some(1) => true,
                _ => return bad("AF metering: 0 auto (the middle), 1 windows"),
            };
            *r.metering_windows.lock() = windows;
            c.af_windows = if windows {
                windows_to(r, &r.windows.lock())
            } else {
                Vec::new()
            };
        }
        ids::AF_WINDOWS => {
            let rects = match value {
                ControlValue::Rect(x) => vec![*x],
                ControlValue::Rects(v) => v.clone(),
                _ => return bad("AF windows: rectangles in output pixels"),
            };
            if *r.metering_windows.lock() {
                c.af_windows = windows_to(r, &rects);
            }
            *r.windows.lock() = rects;
        }
        ids::AF_STATE => return bad("AF state is read only"),
        _ => return None,
    }
    Some(Ok(()))
}

/// Reads an AF control (`None` when `id` is not one).
pub(crate) fn read(r: &AfReport, id: ControlId, c: &Controls) -> Option<ControlValue> {
    if !r.lens.load(Ordering::Acquire) {
        return None;
    }
    Some(match id {
        ids::AF_MODE => ControlValue::Int(match c.af_mode {
            AfMode::Manual => 0,
            AfMode::Auto => 1,
            AfMode::Continuous => 2,
        }),
        ids::AF_STATE => ControlValue::Int(r.state.load(Ordering::Acquire)),
        ids::AF_RANGE => ControlValue::Int(c.af_range as i32),
        ids::AF_SPEED => ControlValue::Int(c.af_speed as i32),
        ids::AF_METERING => ControlValue::Int(i32::from(*r.metering_windows.lock())),
        ids::AF_WINDOWS => ControlValue::Rects(r.windows.lock().clone()),
        ids::LENS_POSITION => {
            let p = f32::from_bits(r.lens_position.load(Ordering::Acquire));
            ControlValue::Float(if p.is_nan() {
                c.lens_position.unwrap_or(0.0) as f32
            } else {
                p
            })
        }
        _ => return None,
    })
}

/// Records what AF made of a frame.
pub(crate) fn report(r: &AfReport, params: &Params) {
    let state = match params.af.state {
        AfState::Idle => 0,
        AfState::Scanning => 1,
        AfState::Focused => 2,
        AfState::Failed => 3,
    };
    r.state.store(state, Ordering::Release);
    if let Some(p) = params.af.lens_position {
        r.lens_position
            .store((p as f32).to_bits(), Ordering::Release);
    }
}

/// The AF controls a camera with a focus lens lists (`limits`: the lens position range in
/// dioptres).
pub(crate) fn metas(limits: (f64, f64)) -> Vec<ControlMeta> {
    let int = |id, name: &str, max: i32, default: i32| ControlMeta {
        id,
        name: name.into(),
        kind: ControlKind::Int,
        access: Access::ReadWrite,
        min: ControlValue::Int(0),
        max: ControlValue::Int(max),
        default: ControlValue::Int(default),
        step: Some(ControlValue::Int(1)),
        menu: None,
        metadata: ControlMetadata::default(),
    };
    let rect = ControlRect {
        x: 0,
        y: 0,
        width: 65535,
        height: 65535,
    };
    vec![
        int(ids::AF_MODE, "af_mode", 2, 2),
        int(ids::AF_TRIGGER, "af_trigger", 1, 0),
        int(ids::AF_RANGE, "af_range", 2, 0),
        int(ids::AF_SPEED, "af_speed", 1, 0),
        int(ids::AF_METERING, "af_metering", 1, 0),
        ControlMeta {
            access: Access::ReadOnly,
            ..int(ids::AF_STATE, "af_state", 3, 0)
        },
        ControlMeta {
            id: ids::LENS_POSITION,
            name: "lens_position".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(limits.0 as f32),
            max: ControlValue::Float(limits.1 as f32),
            default: ControlValue::Float(1.0),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: ids::AF_WINDOWS,
            name: "af_windows".into(),
            kind: ControlKind::Rectangle,
            access: Access::ReadWrite,
            min: ControlValue::Rect(ControlRect {
                width: 1,
                height: 1,
                ..rect
            }),
            max: ControlValue::Rect(rect),
            default: ControlValue::Rects(Vec::new()),
            step: None,
            menu: None,
            metadata: ControlMetadata::default(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn af_controls_reach_the_loop_and_report_back() {
        let r = AfReport::default();
        let mut c = Controls::default();
        assert!(matches!(
            apply(&r, ids::AF_MODE, &ControlValue::Int(1), &mut c),
            Some(Err(CaptureError::ControlUnsupported))
        ));
        assert!(read(&r, ids::AF_STATE, &c).is_none());
        r.set_lens(true);
        r.set_output(1000, 500);
        let ok = |v: Option<Result<(), CaptureError>>| v.unwrap().unwrap();
        ok(apply(&r, ids::AF_MODE, &ControlValue::Int(1), &mut c));
        assert_eq!(c.af_mode, AfMode::Auto);
        ok(apply(&r, ids::AF_TRIGGER, &ControlValue::Int(0), &mut c));
        ok(apply(&r, ids::AF_TRIGGER, &ControlValue::Int(0), &mut c));
        ok(apply(&r, ids::AF_TRIGGER, &ControlValue::Int(1), &mut c));
        assert_eq!((c.af_trigger, c.af_cancel), (2, 1));
        // Windows apply once metering uses them, as fractions of the output.
        let rect = ControlRect {
            x: 100,
            y: 50,
            width: 200,
            height: 100,
        };
        ok(apply(
            &r,
            ids::AF_WINDOWS,
            &ControlValue::Rect(rect),
            &mut c,
        ));
        assert!(c.af_windows.is_empty());
        ok(apply(&r, ids::AF_METERING, &ControlValue::Int(1), &mut c));
        assert_eq!(c.af_windows, [AfWindow::new(0.1, 0.1, 0.2, 0.2)]);
        ok(apply(
            &r,
            ids::LENS_POSITION,
            &ControlValue::Float(2.5),
            &mut c,
        ));
        assert_eq!(c.lens_position, Some(2.5));
        assert!(
            apply(&r, ids::AF_MODE, &ControlValue::Int(3), &mut c)
                .unwrap()
                .is_err()
        );
        assert!(
            apply(&r, ids::AF_STATE, &ControlValue::Int(1), &mut c)
                .unwrap()
                .is_err()
        );
        assert!(apply(&r, ControlId(1), &ControlValue::Int(1), &mut c).is_none());
        let mut p = Params::default();
        p.af.state = AfState::Focused;
        p.af.lens_position = Some(2.25);
        report(&r, &p);
        assert_eq!(read(&r, ids::AF_STATE, &c), Some(ControlValue::Int(2)));
        assert_eq!(
            read(&r, ids::LENS_POSITION, &c),
            Some(ControlValue::Float(2.25))
        );
        assert_eq!(read(&r, ids::AF_MODE, &c), Some(ControlValue::Int(1)));
        assert_eq!(metas((0.0, 15.0)).len(), 8);
    }
}

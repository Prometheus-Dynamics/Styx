//! AE's limits and modes as native controls: the longest exposure and the highest gain AE may
//! use, the tuning's exposure and metering modes, and a metering region (set a metering mode
//! to go back to its weights).

use parking_lot::Mutex;
use styx_core::prelude::{ControlId, ControlRect, ControlValue};
use styx_pipeline::styx_algo::{Controls, MeteringRegion};

use super::super::native_backend::controls as ids;
use crate::capture_api::CaptureError;

/// The exposure modes, by index (the names the tunings use).
pub(crate) const EXPOSURE_MODES: [&str; 3] = ["normal", "short", "long"];
/// The metering modes, by index (the names the tunings use).
pub(crate) const METERING_MODES: [&str; 3] = ["centre-weighted", "spot", "matrix"];

/// Whether `id` is one of these controls.
pub(crate) fn is_ae(id: ControlId) -> bool {
    [
        ids::AE_EXPOSURE_MAX_US,
        ids::AE_GAIN_MAX,
        ids::AE_EXPOSURE_MODE,
        ids::AE_METERING_MODE,
        ids::AE_METERING_REGION,
    ]
    .contains(&id)
}

/// The metering region as set, in output pixels (zero size: none).
#[derive(Debug)]
pub(crate) struct AeRegion(Mutex<ControlRect>);

impl Default for AeRegion {
    fn default() -> Self {
        Self(Mutex::new(ControlRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }))
    }
}

fn bad(what: &str) -> Option<Result<(), CaptureError>> {
    Some(Err(CaptureError::control_apply(what)))
}

fn mode_index(names: &[&str], name: Option<&str>) -> i32 {
    name.and_then(|n| names.iter().position(|m| *m == n))
        .unwrap_or(0) as i32
}

/// Applies one of these controls to `c` (`None` when `id` is not one); `output`: the output
/// size regions are given in.
pub(crate) fn apply(
    region: &AeRegion,
    output: (u32, u32),
    id: ControlId,
    value: &ControlValue,
    c: &mut Controls,
) -> Option<Result<(), CaptureError>> {
    let number = match value {
        ControlValue::Int(v) => Some(f64::from(*v)),
        ControlValue::Uint(v) => Some(f64::from(*v)),
        ControlValue::Float(v) => Some(f64::from(*v)),
        _ => None,
    };
    match id {
        ids::AE_EXPOSURE_MAX_US => match number {
            Some(us) if us >= 0.0 => {
                c.exposure_max = (us > 0.0).then(|| std::time::Duration::from_secs_f64(us / 1e6))
            }
            _ => return bad("AE exposure maximum: microseconds, 0 for none"),
        },
        ids::AE_GAIN_MAX => match number {
            Some(g) if g == 0.0 || g >= 1.0 => c.gain_max = (g > 0.0).then_some(g),
            _ => return bad("AE gain maximum: a gain of at least 1, 0 for none"),
        },
        ids::AE_EXPOSURE_MODE | ids::AE_METERING_MODE => {
            let names = if id == ids::AE_EXPOSURE_MODE {
                &EXPOSURE_MODES
            } else {
                &METERING_MODES
            };
            let Some(name) = number
                .filter(|v| v.fract() == 0.0 && *v >= 0.0)
                .and_then(|v| names.get(v as usize))
            else {
                return bad(&format!("AE mode: one of {names:?}, by index"));
            };
            let slot = if id == ids::AE_EXPOSURE_MODE {
                &mut c.exposure_mode
            } else {
                &mut c.metering_mode
            };
            *slot = Some((*name).to_string());
            // A metering mode replaces a region.
            if id == ids::AE_METERING_MODE {
                c.metering_region = None;
                region.0.lock().width = 0;
            }
        }
        ids::AE_METERING_REGION => {
            let r = match value {
                ControlValue::Rect(r) => r,
                ControlValue::Rects(v) if v.len() == 1 => &v[0],
                _ => return bad("AE metering region: a rectangle in output pixels"),
            };
            let (w, h) = (f64::from(output.0.max(1)), f64::from(output.1.max(1)));
            c.metering_region = (r.width > 0 && r.height > 0).then(|| MeteringRegion {
                x: f64::from(r.x.max(0)) / w,
                y: f64::from(r.y.max(0)) / h,
                width: f64::from(r.width) / w,
                height: f64::from(r.height) / h,
            });
            *region.0.lock() = *r;
        }
        _ => return None,
    }
    Some(Ok(()))
}

/// Reads one of these controls (`None` when `id` is not one).
pub(crate) fn read(region: &AeRegion, id: ControlId, c: &Controls) -> Option<ControlValue> {
    Some(match id {
        ids::AE_EXPOSURE_MAX_US => {
            ControlValue::Uint(c.exposure_max.map_or(0, |d| d.as_micros() as u32))
        }
        ids::AE_GAIN_MAX => ControlValue::Float(c.gain_max.unwrap_or(0.0) as f32),
        ids::AE_EXPOSURE_MODE => {
            ControlValue::Int(mode_index(&EXPOSURE_MODES, c.exposure_mode.as_deref()))
        }
        ids::AE_METERING_MODE => {
            ControlValue::Int(mode_index(&METERING_MODES, c.metering_mode.as_deref()))
        }
        ids::AE_METERING_REGION => ControlValue::Rect(*region.0.lock()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_modes_and_region_reach_the_loop_and_read_back() {
        let (region, mut c) = (AeRegion::default(), Controls::default());
        let set = |c: &mut Controls, id, v| apply(&region, (1000, 500), id, &v, c).unwrap();
        set(&mut c, ids::AE_EXPOSURE_MAX_US, ControlValue::Uint(4000)).unwrap();
        assert_eq!(c.exposure_max, Some(std::time::Duration::from_millis(4)));
        set(&mut c, ids::AE_GAIN_MAX, ControlValue::Float(6.0)).unwrap();
        assert_eq!(c.gain_max, Some(6.0));
        assert!(set(&mut c, ids::AE_GAIN_MAX, ControlValue::Float(0.5)).is_err());
        set(&mut c, ids::AE_EXPOSURE_MODE, ControlValue::Int(1)).unwrap();
        assert_eq!(c.exposure_mode.as_deref(), Some("short"));
        set(&mut c, ids::AE_METERING_MODE, ControlValue::Int(2)).unwrap();
        assert_eq!(c.metering_mode.as_deref(), Some("matrix"));
        assert!(set(&mut c, ids::AE_METERING_MODE, ControlValue::Int(3)).is_err());
        let rect = ControlRect {
            x: 500,
            y: 0,
            width: 500,
            height: 250,
        };
        set(&mut c, ids::AE_METERING_REGION, ControlValue::Rect(rect)).unwrap();
        assert_eq!(
            c.metering_region,
            Some(MeteringRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 0.5
            })
        );
        assert_eq!(
            read(&region, ids::AE_METERING_REGION, &c),
            Some(ControlValue::Rect(rect))
        );
        assert_eq!(
            read(&region, ids::AE_EXPOSURE_MODE, &c),
            Some(ControlValue::Int(1))
        );
        // Zero: no limit; a metering mode replaces the region.
        set(&mut c, ids::AE_EXPOSURE_MAX_US, ControlValue::Uint(0)).unwrap();
        assert_eq!(c.exposure_max, None);
        set(&mut c, ids::AE_METERING_REGION, ControlValue::Rect(rect)).unwrap();
        set(&mut c, ids::AE_METERING_MODE, ControlValue::Int(0)).unwrap();
        assert_eq!(c.metering_region, None);
        let none = ControlRect { width: 0, ..rect };
        set(&mut c, ids::AE_METERING_REGION, ControlValue::Rect(none)).unwrap();
        assert_eq!(c.metering_region, None);
    }
}

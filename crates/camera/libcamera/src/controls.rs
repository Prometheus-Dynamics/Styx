//! The libcamera control mapping: which libcamera control each Styx control uses, and the range
//! conversions between the two. Tables and arithmetic only, so it builds and is tested without
//! libcamera. The backend applies it to a camera.
//!
//! Libcamera's controls are named (`Brightness`, `ExposureTime`, ...) and typed. Their numeric ids
//! come from the library at run time (`styx_control_id`), so the tables here use the names.

use styx_controls::{Control, ControlValue, DeviceKind, Rect};

use crate::{MenuItem, StandardControl, StandardRange};

/// libcamera's control types, as the C API numbers them (`libcamera_control_type`).
pub(crate) const TYPE_BOOL: u32 = 1;
pub(crate) const TYPE_INT32: u32 = 5;
pub(crate) const TYPE_INT64: u32 = 6;
pub(crate) const TYPE_FLOAT: u32 = 7;
pub(crate) const TYPE_RECTANGLE: u32 = 9;

/// `AfModeManual` and `AfModeContinuous` (libcamera's `AfModeEnum`).
pub(crate) const AF_MODE_MANUAL: i32 = 0;
pub(crate) const AF_MODE_CONTINUOUS: i32 = 2;

/// Each libcamera control a Styx control uses, by libcamera's name. A control listed under two
/// Styx controls (`ScalerCrop`, for crop and for zoom) is mapped to its first.
pub(crate) const MAPPED: &[(&str, Control)] = &[
    ("Brightness", Control::Brightness),
    ("Contrast", Control::Contrast),
    ("Saturation", Control::Saturation),
    ("Sharpness", Control::Sharpness),
    ("Gamma", Control::Gamma),
    ("AeEnable", Control::Exposure),
    ("ExposureTime", Control::Exposure),
    ("AnalogueGain", Control::Gain),
    ("AwbEnable", Control::WhiteBalance),
    ("LensPosition", Control::Focus),
    ("AfMode", Control::Focus),
    ("ScalerCrop", Control::Crop),
];

/// The Styx control a libcamera control is mapped to, by its name.
pub(crate) fn mapped_control(name: &str) -> Option<Control> {
    MAPPED
        .iter()
        .find(|(libcamera, _)| *libcamera == name)
        .map(|&(_, control)| control)
}

/// The kind a libcamera control of type `ty` is listed as. A menu is an `Int32` with enumerators.
pub(crate) fn device_kind(ty: u32, menu: bool) -> DeviceKind {
    match ty {
        TYPE_BOOL => DeviceKind::Boolean,
        TYPE_INT32 if menu => DeviceKind::Menu,
        TYPE_INT32 => DeviceKind::Integer,
        TYPE_INT64 => DeviceKind::Integer64,
        _ => DeviceKind::Other,
    }
}

/// One control of a camera's registry, as read after the camera is configured (libcamera narrows
/// ranges to the configuration). Numbers are `f64`: a float control's range is kept whole, and an
/// integer's is exact in it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Listed {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) ty: u32,
    /// libcamera names values for it (enumerators), or its `ControlInfo` lists the values it takes.
    pub(crate) menu: bool,
    /// The camera takes a value for it.
    pub(crate) input: bool,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) default: f64,
    pub(crate) entries: Vec<MenuItem>,
}

impl Listed {
    pub(crate) fn kind(&self) -> DeviceKind {
        device_kind(self.ty, self.menu)
    }
}

/// A menu's entries: `values` (what the camera's `ControlInfo` lists) when there are any, else every
/// enumerator libcamera names; each labelled with its enumerator's name where there is one.
pub(crate) fn menu_entries(values: &[i64], enumerators: &[(i32, String)]) -> Vec<MenuItem> {
    let name_of = |value: i64| {
        enumerators
            .iter()
            .find(|(key, _)| i64::from(*key) == value)
            .map(|(_, name)| name.clone())
    };
    if values.is_empty() {
        enumerators
            .iter()
            .map(|(key, name)| MenuItem {
                value: i64::from(*key),
                name: Some(name.clone()),
            })
            .collect()
    } else {
        values
            .iter()
            .map(|&value| MenuItem {
                value,
                name: name_of(value),
            })
            .collect()
    }
}

/// `v` as a `u32`, clamped (libcamera's integers are signed).
fn clamp_u32(v: f64) -> u32 {
    v.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

/// A libcamera float (gain, gamma) in thousandths.
fn milli(v: f64) -> u32 {
    clamp_u32(v * 1_000.0)
}

/// A libcamera float over `min..=max` as a normalised value from 0 to 1000 (the inverse of
/// [`normalised_to_float`]).
pub(crate) fn float_to_normalised(v: f64, min: f64, max: f64) -> u32 {
    if max <= min {
        return 0;
    }
    clamp_u32(((v - min) / (max - min)).clamp(0.0, 1.0) * 1_000.0)
}

/// A lens position in dioptres as a focus position from 0 (nearest) to 1000 (farthest), the inverse
/// of [`lens_position`].
pub(crate) fn focus_of_lens(v: f64, min: f64, max: f64) -> u32 {
    if max <= min {
        return 0;
    }
    clamp_u32(((max - v) / (max - min)).clamp(0.0, 1.0) * 1_000.0)
}

/// The Styx controls the backend sets on a camera whose registry is `listed`, in Styx units, in
/// [`Control::ALL`] order. A control is listed when the backend's plan for it can be carried out:
/// its libcamera controls are registered (zoom also needs the camera's `ScalerCropMaximum`,
/// `crop_max`). Ranges are the camera's, converted: exposure is libcamera's microseconds as they are,
/// gain and gamma floats in thousandths, normalised controls 0..1000 over the camera's float range,
/// focus 0 (nearest) to 1000 (farthest) over its lens positions. libcamera has no step.
pub(crate) fn standard(listed: &[Listed], crop_max: Option<Rect>) -> Vec<StandardControl> {
    let find = |name: &str| listed.iter().find(|l| l.name == name);
    let blank = |control: Control, read_only: bool| StandardControl {
        control,
        range: None,
        default: None,
        auto: false,
        read_only,
    };
    let manual = |min: u32, max: u32| Some(StandardRange::Manual { min, max });
    let mut out = Vec::new();
    for control in Control::ALL {
        let c = match control {
            Control::Exposure => {
                // Both of the plan's writes turn AeEnable: without it nothing is set.
                let Some(ae) = find("AeEnable") else {
                    continue;
                };
                let time = find("ExposureTime");
                StandardControl {
                    range: time.and_then(|t| manual(clamp_u32(t.min), clamp_u32(t.max))),
                    default: match time {
                        _ if ae.default != 0.0 => Some(ControlValue::Auto),
                        Some(t) => Some(ControlValue::Manual(clamp_u32(t.default))),
                        None => None,
                    },
                    auto: true,
                    ..blank(control, !ae.input)
                }
            }
            Control::Gain => {
                let Some(gain) = find("AnalogueGain") else {
                    continue;
                };
                StandardControl {
                    range: manual(milli(gain.min), milli(gain.max)),
                    default: Some(ControlValue::Manual(milli(gain.default))),
                    ..blank(control, !gain.input)
                }
            }
            Control::WhiteBalance => {
                // Manual white balance is colour gains in libcamera, which Styx does not model:
                // Auto only, no range.
                let Some(awb) = find("AwbEnable") else {
                    continue;
                };
                StandardControl {
                    default: (awb.default != 0.0).then_some(ControlValue::Auto),
                    auto: true,
                    ..blank(control, !awb.input)
                }
            }
            Control::Focus => {
                let Some(af) = find("AfMode") else {
                    continue;
                };
                let lens = find("LensPosition");
                let continuous = af.default as i32 == AF_MODE_CONTINUOUS;
                StandardControl {
                    range: lens.and(manual(0, 1_000)),
                    default: match lens {
                        _ if continuous => Some(ControlValue::Auto),
                        Some(l) => {
                            Some(ControlValue::Manual(focus_of_lens(l.default, l.min, l.max)))
                        }
                        None => None,
                    },
                    auto: true,
                    ..blank(control, !af.input)
                }
            }
            Control::Brightness | Control::Contrast | Control::Saturation | Control::Sharpness => {
                let name = match control {
                    Control::Brightness => "Brightness",
                    Control::Contrast => "Contrast",
                    Control::Saturation => "Saturation",
                    _ => "Sharpness",
                };
                let Some(l) = find(name) else { continue };
                StandardControl {
                    range: manual(0, 1_000),
                    default: Some(ControlValue::Manual(float_to_normalised(
                        l.default, l.min, l.max,
                    ))),
                    ..blank(control, !l.input)
                }
            }
            Control::Gamma => {
                // Styx's gamma is 1000..3000; the camera's range narrows it.
                let Some(l) = find("Gamma") else { continue };
                let within = |v: f64| milli(v).clamp(1_000, 3_000);
                StandardControl {
                    range: manual(within(l.min), within(l.max)),
                    default: Some(ControlValue::Manual(within(l.default))),
                    ..blank(control, !l.input)
                }
            }
            Control::Crop => {
                let Some(l) = find("ScalerCrop") else {
                    continue;
                };
                StandardControl {
                    range: Some(StandardRange::Rect),
                    default: crop_max.map(|_| ControlValue::Crop(None)),
                    ..blank(control, !l.input)
                }
            }
            Control::Zoom => {
                let Some(l) = find("ScalerCrop") else {
                    continue;
                };
                if crop_max.is_none() {
                    continue;
                }
                StandardControl {
                    range: manual(0, 1_000),
                    default: Some(ControlValue::Manual(0)),
                    ..blank(control, !l.input)
                }
            }
            _ => continue,
        };
        out.push(c);
    }
    out
}

/// A normalised value (0 through 1000) to a libcamera float over the camera's `min..=max`.
pub(crate) fn normalised_to_float(value: u32, min: f32, max: f32) -> f32 {
    let t = value.min(1_000) as f32 / 1_000.0;
    min + t * (max - min)
}

/// Gamma in thousandths (1000 through 3000) to libcamera's gamma (1.0 through 3.0).
pub(crate) fn gamma_to_float(value: u32) -> f32 {
    value.clamp(1_000, 3_000) as f32 / 1_000.0
}

/// A focus position (0 nearest, 1000 farthest) to libcamera's lens position in dioptres. Zero is
/// infinity, so the farthest focus is the camera's smallest lens position.
pub(crate) fn lens_position(focus: u32, min: f32, max: f32) -> f32 {
    let t = focus.min(1_000) as f32 / 1_000.0;
    max - t * (max - min)
}

/// The crop for a zoom from 0 (the whole pixel array) to 1000 (four times in each direction, a
/// quarter of the array), centred in `full`.
pub(crate) fn zoom_crop(full: Rect, zoom: u32) -> Rect {
    let scale = 1_000 + 3 * u64::from(zoom.min(1_000));
    let width = (u64::from(full.width) * 1_000 / scale).clamp(1, u64::from(full.width)) as u32;
    let height = (u64::from(full.height) * 1_000 / scale).clamp(1, u64::from(full.height)) as u32;
    Rect {
        x: full.x + (full.width - width) / 2,
        y: full.y + (full.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 4_056,
            height: 3_040,
        }
    }

    #[test]
    fn every_mapped_name_is_a_libcamera_name_and_unique() {
        let mut names: Vec<&str> = MAPPED.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        // ScalerCrop appears once; zoom is reached through it, not listed twice.
        assert_eq!(names.len(), before);
        for name in names {
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric()), "{name}");
            assert!(name.starts_with(|c: char| c.is_ascii_uppercase()), "{name}");
        }
    }

    #[test]
    fn the_names_map_to_the_styx_controls_they_serve() {
        assert_eq!(mapped_control("Brightness"), Some(Control::Brightness));
        assert_eq!(mapped_control("ExposureTime"), Some(Control::Exposure));
        assert_eq!(mapped_control("AeEnable"), Some(Control::Exposure));
        assert_eq!(mapped_control("AwbEnable"), Some(Control::WhiteBalance));
        assert_eq!(mapped_control("LensPosition"), Some(Control::Focus));
        assert_eq!(mapped_control("AfMode"), Some(Control::Focus));
        assert_eq!(mapped_control("ScalerCrop"), Some(Control::Crop));
        assert_eq!(mapped_control("AnalogueGain"), Some(Control::Gain));
        // Read-only and unused controls are listed, not mapped.
        assert_eq!(mapped_control("ColourTemperature"), None);
        assert_eq!(mapped_control("ColourGains"), None);
        assert_eq!(mapped_control("FrameDuration"), None);
    }

    #[test]
    fn scaler_crop_is_the_crop_and_zoom_is_set_through_it() {
        assert_eq!(mapped_control("ScalerCrop"), Some(Control::Crop));
        assert!(MAPPED.iter().all(|&(_, control)| control != Control::Zoom));
    }

    #[test]
    fn listed_kinds_follow_the_libcamera_type_and_menus() {
        assert_eq!(device_kind(TYPE_BOOL, false), DeviceKind::Boolean);
        assert_eq!(device_kind(TYPE_INT32, false), DeviceKind::Integer);
        assert_eq!(device_kind(TYPE_INT32, true), DeviceKind::Menu);
        assert_eq!(device_kind(TYPE_INT64, false), DeviceKind::Integer64);
        assert_eq!(device_kind(TYPE_FLOAT, false), DeviceKind::Other);
        assert_eq!(device_kind(TYPE_RECTANGLE, false), DeviceKind::Other);
    }

    #[test]
    fn normalised_values_span_the_camera_range() {
        // libcamera's brightness is -1.0 through 1.0.
        assert_eq!(normalised_to_float(0, -1.0, 1.0), -1.0);
        assert_eq!(normalised_to_float(500, -1.0, 1.0), 0.0);
        assert_eq!(normalised_to_float(1_000, -1.0, 1.0), 1.0);
        assert_eq!(normalised_to_float(5_000, -1.0, 1.0), 1.0, "clamped");
        // Contrast is 0 through 32.
        assert!((normalised_to_float(250, 0.0, 32.0) - 8.0).abs() < 1e-5);
    }

    #[test]
    fn gamma_is_thousandths_of_libcameras_gamma() {
        assert_eq!(gamma_to_float(2_200), 2.2);
        assert_eq!(gamma_to_float(900), 1.0, "clamped to 1.0");
        assert_eq!(gamma_to_float(9_000), 3.0, "clamped to 3.0");
    }

    #[test]
    fn lens_position_runs_nearest_at_the_top_of_the_range() {
        // A camera reporting 0.0 (infinity) through 10.0 dioptres.
        assert_eq!(lens_position(0, 0.0, 10.0), 10.0, "0 is the nearest focus");
        assert_eq!(lens_position(1_000, 0.0, 10.0), 0.0, "1000 is the farthest");
        assert_eq!(lens_position(500, 0.0, 10.0), 5.0);
    }

    #[test]
    fn zoom_is_a_centred_crop_down_to_a_quarter() {
        assert_eq!(zoom_crop(full(), 0), full(), "no zoom is the whole array");
        let four = zoom_crop(full(), 1_000);
        assert_eq!((four.width, four.height), (1_014, 760));
        assert_eq!((four.x, four.y), (1_521, 1_140), "centred");
        let mid = zoom_crop(full(), 500);
        assert!(mid.width < full().width && mid.width > four.width);
        assert!(
            (mid.x + mid.width / 2).abs_diff(full().width / 2) <= 1,
            "centred"
        );
    }

    #[test]
    fn zoom_stays_inside_the_array_and_never_empties() {
        for z in [0u32, 1, 250, 999, 1_000, 4_000] {
            let crop = zoom_crop(full(), z);
            assert!(crop.width >= 1 && crop.height >= 1);
            assert!(crop.x + crop.width <= full().width);
            assert!(crop.y + crop.height <= full().height);
        }
    }

    fn listed(name: &str, ty: u32, min: f64, max: f64, default: f64) -> Listed {
        Listed {
            id: 0,
            name: name.into(),
            ty,
            menu: false,
            input: true,
            min,
            max,
            default,
            entries: Vec::new(),
        }
    }

    fn of(standard: &[StandardControl], control: Control) -> Option<StandardControl> {
        standard.iter().copied().find(|c| c.control == control)
    }

    #[test]
    fn menu_entries_are_the_listed_values_named_by_their_enumerators() {
        let names = [
            (0, "Off".to_string()),
            (1, "On".to_string()),
            (2, "Fast".to_string()),
        ];
        // The camera lists two of libcamera's three values, and one libcamera has no name for.
        let entries = menu_entries(&[0, 2, 7], &names);
        assert_eq!(
            entries,
            [
                MenuItem {
                    value: 0,
                    name: Some("Off".into())
                },
                MenuItem {
                    value: 2,
                    name: Some("Fast".into())
                },
                MenuItem {
                    value: 7,
                    name: None
                },
            ]
        );
        // No values listed: every enumerator.
        let entries = menu_entries(&[], &names);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[1],
            MenuItem {
                value: 1,
                name: Some("On".into())
            }
        );
        assert!(menu_entries(&[], &[]).is_empty());
    }

    #[test]
    fn the_inverse_conversions_return_the_styx_value() {
        for v in [0u32, 1, 250, 500, 999, 1_000] {
            let f = f64::from(normalised_to_float(v, -1.0, 1.0));
            assert!(float_to_normalised(f, -1.0, 1.0).abs_diff(v) <= 1, "{v}");
            let lens = f64::from(lens_position(v, 0.0, 15.0));
            assert!(focus_of_lens(lens, 0.0, 15.0).abs_diff(v) <= 1, "{v}");
        }
        assert_eq!(float_to_normalised(3.0, 1.0, 1.0), 0);
        assert_eq!(float_to_normalised(-9.0, -1.0, 1.0), 0);
        assert_eq!(float_to_normalised(9.0, -1.0, 1.0), 1_000);
    }

    /// The camera's configured ranges in Styx units: exposure in microseconds as libcamera has it,
    /// gain in thousandths, normalised controls over the camera's float range, gamma within Styx's
    /// 1000..3000, focus over the lens positions; Auto defaults where the auto control defaults on.
    #[test]
    fn standard_controls_are_the_cameras_ranges_in_styx_units() {
        let list = vec![
            listed("AeEnable", TYPE_BOOL, 0.0, 1.0, 1.0),
            listed("ExposureTime", TYPE_INT32, 14.0, 66_666.0, 20_000.0),
            listed("AnalogueGain", TYPE_FLOAT, 1.0, 15.875, 1.0),
            listed("AwbEnable", TYPE_BOOL, 0.0, 1.0, 1.0),
            listed("Brightness", TYPE_FLOAT, -1.0, 1.0, 0.0),
            listed("Contrast", TYPE_FLOAT, 0.0, 32.0, 1.0),
            listed("Gamma", TYPE_FLOAT, 0.5, 2.2, 2.2),
            listed("AfMode", TYPE_INT32, 0.0, 2.0, 2.0),
            listed("LensPosition", TYPE_FLOAT, 0.0, 15.0, 1.0),
            listed("ScalerCrop", TYPE_RECTANGLE, 0.0, 0.0, 0.0),
            // Listed, not mapped: no standard line.
            listed("ColourTemperature", TYPE_INT32, 100.0, 100_000.0, 5_000.0),
        ];
        let standard = standard(&list, Some(full()));
        let exposure = of(&standard, Control::Exposure).unwrap();
        assert_eq!(
            exposure.range,
            Some(StandardRange::Manual {
                min: 14,
                max: 66_666
            })
        );
        assert_eq!(exposure.default, Some(ControlValue::Auto));
        assert!(exposure.auto && !exposure.read_only);

        let gain = of(&standard, Control::Gain).unwrap();
        assert_eq!(
            gain.range,
            Some(StandardRange::Manual {
                min: 1_000,
                max: 15_875
            })
        );
        assert_eq!(gain.default, Some(ControlValue::Manual(1_000)));
        assert!(!gain.auto);

        let wb = of(&standard, Control::WhiteBalance).unwrap();
        assert_eq!(
            (wb.range, wb.default, wb.auto),
            (None, Some(ControlValue::Auto), true)
        );

        let brightness = of(&standard, Control::Brightness).unwrap();
        assert_eq!(
            brightness.range,
            Some(StandardRange::Manual { min: 0, max: 1_000 })
        );
        assert_eq!(brightness.default, Some(ControlValue::Manual(500)));
        let contrast = of(&standard, Control::Contrast).unwrap();
        assert_eq!(contrast.default, Some(ControlValue::Manual(31)));

        let gamma = of(&standard, Control::Gamma).unwrap();
        assert_eq!(
            gamma.range,
            Some(StandardRange::Manual {
                min: 1_000,
                max: 2_200
            })
        );
        assert_eq!(gamma.default, Some(ControlValue::Manual(2_200)));

        let focus = of(&standard, Control::Focus).unwrap();
        assert_eq!(
            focus.range,
            Some(StandardRange::Manual { min: 0, max: 1_000 })
        );
        assert_eq!(focus.default, Some(ControlValue::Auto));

        let crop = of(&standard, Control::Crop).unwrap();
        assert_eq!(crop.range, Some(StandardRange::Rect));
        assert_eq!(crop.default, Some(ControlValue::Crop(None)));
        let zoom = of(&standard, Control::Zoom).unwrap();
        assert_eq!(
            zoom.range,
            Some(StandardRange::Manual { min: 0, max: 1_000 })
        );

        // Not registered or not mapped: left out, not shown with a generic range.
        for absent in [Control::Saturation, Control::Sharpness, Control::FrameRate] {
            assert_eq!(of(&standard, absent), None, "{absent:?}");
        }
        let ordered: Vec<Control> = standard.iter().map(|c| c.control).collect();
        let mut sorted = ordered.clone();
        sorted.sort_by_key(|c| c.index());
        assert_eq!(ordered, sorted);
    }

    /// A control is listed only when the backend's plan for it can run: exposure needs AeEnable,
    /// a manual focus needs AfMode and LensPosition, zoom needs the crop maximum.
    #[test]
    fn a_standard_control_is_listed_only_when_it_can_be_set() {
        let only_time = [listed("ExposureTime", TYPE_INT32, 1.0, 10.0, 5.0)];
        assert_eq!(of(&standard(&only_time, None), Control::Exposure), None);

        let manual_ae = [
            listed("AeEnable", TYPE_BOOL, 0.0, 1.0, 0.0),
            listed("ExposureTime", TYPE_INT32, 1.0, 10.0, 5.0),
        ];
        let exposure = of(&standard(&manual_ae, None), Control::Exposure).unwrap();
        assert_eq!(exposure.default, Some(ControlValue::Manual(5)));

        let af_only = [listed("AfMode", TYPE_INT32, 0.0, 2.0, 0.0)];
        let focus = of(&standard(&af_only, None), Control::Focus).unwrap();
        assert_eq!((focus.range, focus.default, focus.auto), (None, None, true));
        assert_eq!(
            of(
                &standard(&[listed("LensPosition", TYPE_FLOAT, 0.0, 15.0, 0.0)], None),
                Control::Focus
            ),
            None
        );

        let crop = [listed("ScalerCrop", TYPE_RECTANGLE, 0.0, 0.0, 0.0)];
        let without_max = standard(&crop, None);
        assert_eq!(of(&without_max, Control::Zoom), None);
        assert_eq!(of(&without_max, Control::Crop).unwrap().default, None);

        let mut read_only = listed("AnalogueGain", TYPE_FLOAT, 1.0, 8.0, 1.0);
        read_only.input = false;
        assert!(
            of(&standard(&[read_only], None), Control::Gain)
                .unwrap()
                .read_only
        );
    }
}

//! The libcamera control mapping: which libcamera control each Styx control uses, and the range
//! conversions between the two. Tables and arithmetic only, so it builds and is tested without
//! libcamera. The backend applies it to a camera.
//!
//! Libcamera's controls are named (`Brightness`, `ExposureTime`, ...) and typed. Their numeric ids
//! come from the library at run time (`styx_control_id`), so the tables here use the names.

use styx_controls::{Control, DeviceKind, Rect};

/// libcamera's control types, as the shim numbers them (`libcamera_control_type`).
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
}

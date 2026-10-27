//! The native camera's control descriptors: ranges from the sensor's modes and timing.

use super::*;

pub(super) fn control_metas(info: &CameraInfo) -> Vec<ControlMeta> {
    let d = &info.description;
    let first = info.modes.first();
    let (exp_min, exp_max, exp_default) = first.map_or((1, 1_000_000, 10_000), |m| {
        let t = &m.timing;
        let l = t.exposure_limits(t.frame_length_max());
        let default = t.lines_to_duration(f64::from(d.controls.exposure.default));
        (
            l.min.as_micros().max(1) as u32,
            l.max.as_micros() as u32,
            default.as_micros() as u32,
        )
    });
    let g = &d.controls.analog_gain;
    let (mut gmin, mut gmax) = (g.gain_for_code(g.min_code), g.gain_for_code(g.max_code));
    if let Some(dg) = &d.controls.digital_gain {
        gmin *= dg.gain_for_code(dg.min_code);
        gmax *= dg.gain_for_code(dg.max_code);
    }
    let (fmin, fmax) = info.modes.iter().fold((f64::MAX, 0.0f64), |(lo, hi), m| {
        (lo.min(m.min_fps()), hi.max(m.max_fps()))
    });
    let us = |f: f64| (1e6 / f.max(1e-6)) as u32;
    let meta = |id, name: &str, kind, min, max, default, step| ControlMeta {
        id,
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        min,
        max,
        default,
        step: Some(step),
        menu: None,
        metadata: ControlMetadata::default(),
    };
    // An `Int` control whose values are `labels`' indices, listed as its menu.
    let menu = |m: ControlMeta, labels: &[&str]| ControlMeta {
        menu: Some(labels.iter().map(|l| l.to_string()).collect()),
        ..m
    };
    vec![
        meta(
            controls::EXPOSURE_TIME_US,
            "exposure_time_us",
            ControlKind::Uint,
            ControlValue::Uint(exp_min),
            ControlValue::Uint(exp_max),
            ControlValue::Uint(exp_default),
            ControlValue::Uint(1),
        ),
        meta(
            controls::GAIN,
            "gain",
            ControlKind::Float,
            ControlValue::Float(gmin as f32),
            ControlValue::Float(gmax as f32),
            ControlValue::Float(gmin as f32),
            ControlValue::Float(0.0625),
        ),
        meta(
            controls::FRAME_DURATION_US,
            "frame_duration_us",
            ControlKind::Uint,
            ControlValue::Uint(us(fmax)),
            ControlValue::Uint(us(fmin)),
            ControlValue::Uint(first.map_or(33_333, |m| us(m.default_interval.fps()))),
            ControlValue::Uint(1),
        ),
        meta(
            controls::FRAME_RATE,
            "frame_rate",
            ControlKind::Float,
            ControlValue::Float(fmin as f32),
            ControlValue::Float(fmax as f32),
            ControlValue::Float(first.map_or(30.0, |m| m.default_interval.fps() as f32)),
            ControlValue::Float(0.001),
        ),
        // The processed modes' 3A loop.
        meta(
            controls::AE_ENABLE,
            "ae_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            controls::EXPOSURE_VALUE,
            "exposure_value",
            ControlKind::Float,
            ControlValue::Float(-8.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.1),
        ),
        meta(
            controls::AWB_ENABLE,
            "awb_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            controls::COLOUR_TEMPERATURE,
            "colour_temperature",
            ControlKind::Uint,
            ControlValue::Uint(1000),
            ControlValue::Uint(20_000),
            ControlValue::Uint(0),
            ControlValue::Uint(1),
        ),
        meta(
            controls::RED_GAIN,
            "red_gain",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.01),
        ),
        meta(
            controls::BLUE_GAIN,
            "blue_gain",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.01),
        ),
        menu(
            meta(
                controls::AE_FLICKER_MODE,
                "ae_flicker_mode",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(3),
                ControlValue::Int(3),
                ControlValue::Int(1),
            ),
            &["off", "50 Hz", "60 Hz", "auto"],
        ),
        menu(
            meta(
                controls::AE_DEFLICKER_MODE,
                "ae_deflicker_mode",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(2),
                ControlValue::Int(2),
                ControlValue::Int(1),
            ),
            &["off", "on", "with flicker avoidance"],
        ),
        meta(
            controls::AE_EXPOSURE_MAX_US,
            "ae_exposure_max_us",
            ControlKind::Uint,
            ControlValue::Uint(0),
            ControlValue::Uint(exp_max),
            ControlValue::Uint(0),
            ControlValue::Uint(1),
        ),
        meta(
            controls::AE_GAIN_MAX,
            "ae_gain_max",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(gmax as f32),
            ControlValue::Float(0.0),
            ControlValue::Float(0.0625),
        ),
        menu(
            meta(
                controls::AE_EXPOSURE_MODE,
                "ae_exposure_mode",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(2),
                ControlValue::Int(0),
                ControlValue::Int(1),
            ),
            &super::super::native_isp::EXPOSURE_MODES,
        ),
        menu(
            meta(
                controls::AE_METERING_MODE,
                "ae_metering_mode",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(2),
                ControlValue::Int(0),
                ControlValue::Int(1),
            ),
            &super::super::native_isp::METERING_MODES,
        ),
        ControlMeta {
            step: None,
            ..meta(
                controls::AE_METERING_REGION,
                "ae_metering_region",
                ControlKind::Rectangle,
                ControlValue::Rect(ControlRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }),
                ControlValue::Rect(ControlRect {
                    x: 0,
                    y: 0,
                    width: 65535,
                    height: 65535,
                }),
                ControlValue::Rect(ControlRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                }),
                ControlValue::None,
            )
        },
        ControlMeta {
            access: Access::ReadOnly,
            ..meta(
                controls::AE_FLICKER_DETECTED,
                "ae_flicker_detected",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(1_000_000),
                ControlValue::Int(0),
                ControlValue::Int(1),
            )
        },
        ControlMeta {
            access: Access::ReadOnly,
            ..menu(
                meta(
                    controls::AE_STATE,
                    "ae_state",
                    ControlKind::Int,
                    ControlValue::Int(0),
                    ControlValue::Int(2),
                    ControlValue::Int(1),
                    ControlValue::Int(1),
                ),
                &["inactive", "searching", "converged"],
            )
        },
    ]
    .into_iter()
    .chain(
        match super::super::native_isp::isp_name(info) {
            "pisp" => Some(super::super::native_isp::crop_metas(info, true)),
            "software" => Some(super::super::native_isp::crop_metas(info, true)),
            _ => None,
        }
        .into_iter()
        .flatten(),
    )
    .chain(info.lens.as_ref().map_or_else(Vec::new, |l| {
        let m = &l.description.map;
        let limits = if m.len() >= 4 {
            (m[0], m[m.len() - 2])
        } else {
            (0.0, 12.0)
        };
        super::super::native_isp::af_metas(limits)
    }))
    .collect()
}

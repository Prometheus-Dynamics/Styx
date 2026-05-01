#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]
use styx_capture::prelude::*;

#[cfg(feature = "probe")]
use libcamera::{
    camera::Camera,
    color_space::{ColorSpace as LcColorSpace, Primaries as LcPrimaries, Range as LcRange},
    control,
    control_value::{ControlType, ControlValue as LcValue},
    controls::ControlId as LcControlId,
    properties::PropertyId,
    stream::StreamRole,
};
#[cfg(feature = "probe")]
use smallvec::smallvec;
#[cfg(feature = "probe")]
use styx_core::controls::{Access, ControlKind, ControlMetadata, ControlValue};

#[cfg(feature = "probe")]
mod manager;
#[cfg(feature = "probe")]
pub use manager::{
    ActiveCameraUse, DEFAULT_LIBCAMERA_PROBE_CACHE_MS, LibcameraManagerConfig, begin_camera_use,
    find_camera, manager_config, set_manager_config, subscribe_hotplug_events, try_stop_if_idle,
};
#[cfg(feature = "probe")]
use manager::{read_probe_cache, with_manager_mut, write_probe_cache};

#[cfg(feature = "probe")]
pub const LIBCAMERA_FRAME_DURATION_LIMITS: styx_core::controls::ControlId =
    styx_core::controls::ControlId(30);
#[cfg(feature = "probe")]
pub const LIBCAMERA_NOISE_REDUCTION_MODE: styx_core::controls::ControlId =
    styx_core::controls::ControlId(10002);

/// Libcamera device information with a descriptor built from advertised formats.
#[derive(Clone)]
pub struct LibcameraDeviceInfo {
    pub id: String,
    pub properties: Vec<(String, String)>,
    pub descriptor: CaptureDescriptor,
}

/// Probe available libcamera devices and return descriptors.
#[cfg(feature = "probe")]
pub fn probe_devices() -> Vec<LibcameraDeviceInfo> {
    probe_devices_with_errors().0
}

/// Probe available libcamera devices and return descriptors plus any probe errors.
#[cfg(feature = "probe")]
pub fn probe_devices_with_errors() -> (Vec<LibcameraDeviceInfo>, Vec<String>) {
    probe_devices_inner(false)
}

/// Probe available libcamera devices and bypass the short-lived cache.
#[cfg(feature = "probe")]
pub fn probe_devices_uncached() -> Vec<LibcameraDeviceInfo> {
    probe_devices_uncached_with_errors().0
}

#[cfg(feature = "probe")]
pub fn probe_devices_uncached_with_errors() -> (Vec<LibcameraDeviceInfo>, Vec<String>) {
    probe_devices_inner(true)
}

#[cfg(feature = "probe")]
fn probe_devices_inner(force_refresh: bool) -> (Vec<LibcameraDeviceInfo>, Vec<String>) {
    if !force_refresh && let Some(cached) = read_probe_cache() {
        return (cached, Vec::new());
    }

    let (devices, errors) = collect_devices();
    write_probe_cache(&devices);
    (devices, errors)
}

#[cfg(feature = "probe")]
fn collect_devices() -> (Vec<LibcameraDeviceInfo>, Vec<String>) {
    if let Some(cached) = read_probe_cache() {
        let _ = cached;
    }

    let (devices, errors) = match with_manager_mut(|manager| {
        let mut devices = Vec::new();
        let mut errors = Vec::new();
        let cameras = manager.cameras();
        if debug_enabled() {
            let ids: Vec<String> = cameras.iter().map(|c| c.id().to_string()).collect();
            tracing::debug!(
                backend = "libcamera",
                camera_count = ids.len(),
                camera_ids = ?ids,
                "libcamera probe discovered cameras"
            );
        }

        for camera in cameras.iter() {
            match build_info(&camera) {
                Ok(info) => devices.push(info),
                Err(err) => {
                    errors.push(format!(
                        "failed to build descriptor for {}: {err}",
                        camera.id()
                    ));
                    if debug_enabled() {
                        tracing::debug!(
                            backend = "libcamera",
                            camera_id = %camera.id(),
                            error = %err,
                            "libcamera probe failed to build descriptor"
                        );
                    }
                }
            }
        }

        (devices, errors)
    }) {
        Ok(result) => result,
        Err(err) => {
            if debug_enabled() {
                tracing::debug!(
                    backend = "libcamera",
                    error = %err,
                    "libcamera manager init failed"
                );
            }
            write_probe_cache(&[]);
            return (
                Vec::new(),
                vec![format!("camera manager init failed: {err}")],
            );
        }
    };
    (devices, errors)
}

#[cfg(feature = "probe")]
fn debug_enabled() -> bool {
    std::env::var_os("STYX_LIBCAMERA_DEBUG").is_some()
}

#[cfg(feature = "probe")]
fn build_info(camera: &Camera) -> Result<LibcameraDeviceInfo, Box<dyn std::error::Error>> {
    let mut modes = Vec::new();
    let mut seen = std::collections::HashSet::<(FourCc, u32, u32)>::new();
    let is_pisp = is_rpi_pisp_sensor_i2c(camera.id());

    // Some pipelines (notably on Raspberry Pi / PiSP) advertise different pixel formats depending
    // on the requested stream role. Probe multiple roles so we surface everything libcamera
    // advertises instead of an implicit allow-list.
    for role in [
        StreamRole::ViewFinder,
        StreamRole::VideoRecording,
        StreamRole::StillCapture,
        StreamRole::Raw,
    ] {
        if let Some(cfg) = camera.generate_configuration(&[role])
            && let Some(view_cfg) = cfg.get(0)
        {
            let color = map_color_space(view_cfg.get_color_space());
            let formats = view_cfg.formats();
            for pf in formats.pixel_formats().into_iter() {
                let fourcc = map_pixel_format_to_fourcc(pf);
                if is_pisp && pisp_disallowed_fourcc(fourcc) {
                    continue;
                }
                for size in formats.sizes(pf) {
                    let Some(res) = Resolution::new(size.width, size.height) else {
                        continue;
                    };
                    if !seen.insert((fourcc, size.width, size.height)) {
                        continue;
                    }
                    let format = MediaFormat::new(fourcc, res, color);
                    modes.push(Mode {
                        id: ModeId {
                            format,
                            interval: None,
                        },
                        format,
                        intervals: smallvec![],
                        interval_stepwise: None,
                    });
                }
            }
        }
    }
    let controls = map_controls(camera.controls());
    let mut properties = map_properties(camera.properties());
    properties.push(("id".into(), camera.id().to_string()));
    let descriptor = CaptureDescriptor { modes, controls };
    Ok(LibcameraDeviceInfo {
        id: camera.id().to_string(),
        properties,
        descriptor,
    })
}

#[cfg(feature = "probe")]
fn is_rpi_pisp_sensor_i2c(id: &str) -> bool {
    id.starts_with("/base/") && id.contains("/i2c@")
}

#[cfg(feature = "probe")]
fn pisp_disallowed_fourcc(code: FourCc) -> bool {
    // PiSP asserts on several formats during configuration validation.
    matches!(
        &code.to_u32().to_le_bytes(),
        b"YV12" | b"XB24" | b"XR24" | b"YU16" | b"YV16" | b"YU24" | b"YV24" | b"YVYU" | b"VYUY"
    )
}

#[cfg(feature = "probe")]
fn map_pixel_format_to_fourcc(pf: libcamera::pixel_format::PixelFormat) -> FourCc {
    let base = FourCc::from(pf.fourcc());
    match base.to_u32().to_le_bytes() {
        // Normalize libcamera's RGB/BGR FourCCs into Styx's "friendly" aliases.
        // This keeps the rest of the stack consistent (encoders/decoders default to `RG24`).
        bytes if bytes == *b"RGB3" => return FourCc::RG24,
        bytes if bytes == *b"BGR3" => return FourCc::BG24,
        bytes if bytes == *b"RGB0" => return FourCc::XR24,
        bytes if bytes == *b"BGR0" => return FourCc::XB24,
        _ => {}
    }
    let Some(info) = pf.info() else {
        return base;
    };
    if !info.packed || info.colour_encoding != libcamera::pixel_format::ColourEncoding::Raw {
        return base;
    }

    const RG10: [u8; 4] = *b"RG10";
    const BG10: [u8; 4] = *b"BG10";
    const GB10: [u8; 4] = *b"GB10";
    const BA10: [u8; 4] = *b"BA10";
    const RG12: [u8; 4] = *b"RG12";
    const BG12: [u8; 4] = *b"BG12";
    const GB12: [u8; 4] = *b"GB12";
    const BA12: [u8; 4] = *b"BA12";

    match (base.to_u32().to_le_bytes(), info.bits_per_pixel) {
        // RAW10 MIPI packed.
        (RG10, 10) => FourCc::new(*b"pRAA"),
        (BG10, 10) => FourCc::new(*b"pBAA"),
        (GB10, 10) => FourCc::new(*b"pGAA"),
        (BA10, 10) => FourCc::new(*b"pgAA"),

        // RAW12 MIPI packed.
        (RG12, 12) => FourCc::new(*b"pRCC"),
        (BG12, 12) => FourCc::new(*b"pBCC"),
        (GB12, 12) => FourCc::new(*b"pGCC"),
        (BA12, 12) => FourCc::new(*b"pgCC"),

        _ => base,
    }
}

#[cfg(feature = "probe")]
fn map_controls(map: &control::ControlInfoMap) -> Vec<ControlMeta> {
    fn value_cardinality(value: &LcValue) -> Option<usize> {
        match value {
            LcValue::None => Some(0),
            LcValue::Bool(v) => Some(v.len()),
            LcValue::Byte(v) => Some(v.len()),
            LcValue::Uint16(v) => Some(v.len()),
            LcValue::Uint32(v) => Some(v.len()),
            LcValue::Int32(v) => Some(v.len()),
            LcValue::Int64(v) => Some(v.len()),
            LcValue::Float(v) => Some(v.len()),
            LcValue::String(v) => Some(v.len()),
            _ => None,
        }
    }

    fn kind_from_type(control_type: ControlType) -> ControlKind {
        match control_type {
            ControlType::Bool => ControlKind::Bool,
            ControlType::Byte | ControlType::Uint16 | ControlType::Uint32 => ControlKind::Uint,
            ControlType::Int32 | ControlType::Int64 => ControlKind::Int,
            ControlType::Float => ControlKind::Float,
            ControlType::None
            | ControlType::String
            | ControlType::Rectangle
            | ControlType::Size
            | ControlType::Point => ControlKind::Unknown,
        }
    }

    fn as_nonneg_i64(v: &ControlValue) -> Option<i64> {
        match v {
            ControlValue::Uint(n) => Some(*n as i64),
            ControlValue::Int(n) if *n >= 0 => Some(*n as i64),
            _ => None,
        }
    }

    let mut out = Vec::new();
    for (id, info) in map.into_iter() {
        let min_raw = info.min();
        let max_raw = info.max();
        let default_raw = info.def();
        let values = info.values();
        let multivalue = [
            value_cardinality(&min_raw),
            value_cardinality(&max_raw),
            value_cardinality(&default_raw),
        ]
        .into_iter()
        .flatten()
        .any(|len| len > 1)
            || values
                .iter()
                .filter_map(value_cardinality)
                .any(|len| len > 1);

        // Prefer dynamic lookup so we include draft/vendor controls (e.g. NoiseReductionMode)
        // that aren't covered by the generated `TryFrom` tables.
        let name = LcControlId::from_id(id)
            .map(|cid| cid.name().to_string())
            .or_else(|| {
                LcControlId::try_from(id)
                    .ok()
                    .map(|cid| cid.name().to_string())
            })
            .unwrap_or_else(|| format!("ctrl_{id}"));
        let min = convert_value(&min_raw);
        let max = convert_value(&max_raw);
        let default = convert_value(&default_raw);
        let control_type = ControlType::from(&default_raw);
        let mut kind = kind_from_type(control_type);

        // If libcamera provides a bounded list of accepted values, treat it as a menu.
        // Note: We only surface a menu when the allowed values are a contiguous 0..N range.
        // This preserves the existing "menu value == index" semantics used by the rest of Styx.
        let mut menu: Option<Vec<String>> = None;
        if !values.is_empty() {
            let mut allowed = values
                .iter()
                .map(convert_value)
                .filter_map(|v| as_nonneg_i64(&v))
                .collect::<Vec<_>>();
            allowed.sort_unstable();
            allowed.dedup();
            let contiguous = allowed.first().is_some_and(|first| *first == 0)
                && allowed.iter().enumerate().all(|(idx, v)| *v == idx as i64);

            if contiguous {
                let enumerators = LcControlId::from_id(id)
                    .map(|cid| cid.enumerators_map())
                    .unwrap_or_default();
                menu = Some(
                    allowed
                        .iter()
                        .map(|v| enumerators.get(&(*v as i32)).cloned().unwrap_or_default())
                        .collect(),
                );
                kind = match kind {
                    ControlKind::Int => ControlKind::IntMenu,
                    ControlKind::Uint => ControlKind::Menu,
                    other => other,
                };
            }
        }

        // libcamera-rs currently doesn't expose some draft controls via `ControlId::from_id`,
        // so patch up well-known PiSP controls by numeric ID.
        let (name, menu, metadata) = match (id, name.as_str(), menu.as_ref()) {
            // libcamera::controls::draft::NoiseReductionMode
            (id, "ctrl_10002", Some(existing))
                if id == LIBCAMERA_NOISE_REDUCTION_MODE.0
                    && existing.iter().all(|s| s.is_empty()) =>
            {
                (
                    "NoiseReductionMode".to_string(),
                    Some(vec![
                        "NoiseReductionModeOff".into(),
                        "NoiseReductionModeFast".into(),
                        "NoiseReductionModeHighQuality".into(),
                        "NoiseReductionModeMinimal".into(),
                        "NoiseReductionModeZSL".into(),
                    ]),
                    ControlMetadata {
                        requires_tdn_output: true,
                    },
                )
            }
            (id, "ctrl_10002", None) if id == LIBCAMERA_NOISE_REDUCTION_MODE.0 => (
                "NoiseReductionMode".to_string(),
                Some(vec![
                    "NoiseReductionModeOff".into(),
                    "NoiseReductionModeFast".into(),
                    "NoiseReductionModeHighQuality".into(),
                    "NoiseReductionModeMinimal".into(),
                    "NoiseReductionModeZSL".into(),
                ]),
                ControlMetadata {
                    requires_tdn_output: true,
                },
            ),
            _ => (name, menu, ControlMetadata::default()),
        };

        // Skip unsupported libcamera control types entirely rather than exposing "Unknown".
        if matches!(kind, ControlKind::Unknown) {
            continue;
        }
        let access = if id == LIBCAMERA_FRAME_DURATION_LIMITS.0 || multivalue {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        };

        out.push(ControlMeta {
            id: ControlId(id),
            name,
            kind,
            // Multi-value libcamera controls (for example `FrameDurationLimits`) cannot be
            // round-tripped safely through Styx's scalar `ControlValue`. Expose them as read-only
            // metadata so they remain visible, but do not feed their flattened values back into
            // libcamera on restart/reconfigure.
            //
            // `FrameDurationLimits` is additionally forced read-only even when libcamera reports a
            // scalar-looking default/min/max, because the live control value is still a two-value
            // span and writing it back as a scalar can wedge or abort restart paths.
            access,
            min,
            max,
            default,
            step: None,
            menu,
            metadata,
        });
    }
    out
}

#[cfg(feature = "probe")]
fn map_properties(props: &control::PropertyList) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (id, val) in props.into_iter() {
        let name = PropertyId::try_from(id)
            .map(|pid| pid.name().to_string())
            .unwrap_or_else(|_| format!("prop_{id}"));
        out.push((name, format_property_value(&val)));
    }
    out
}

#[cfg(feature = "probe")]
fn format_property_value(val: &LcValue) -> String {
    match val {
        LcValue::None => String::new(),
        LcValue::Bool(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Byte(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Uint16(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Uint32(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Int32(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Int64(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::Float(v) => v.first().map(|n| n.to_string()).unwrap_or_default(),
        LcValue::String(v) => v.first().cloned().unwrap_or_default(),
        other => format!("{other:?}"),
    }
}

#[cfg(feature = "probe")]
fn map_color_space(cs: Option<LcColorSpace>) -> ColorSpace {
    let Some(cs) = cs else {
        return ColorSpace::Unknown;
    };
    let primaries = cs.primaries;
    let transfer = cs.transfer_function;
    let range = cs.range;
    let full = matches!(range, LcRange::Full);
    match (primaries, transfer) {
        (LcPrimaries::Rec2020, _) => {
            if full {
                ColorSpace::Srgb
            } else {
                ColorSpace::Bt2020
            }
        }
        (LcPrimaries::Rec709 | LcPrimaries::Smpte170m, _)
        | (_, libcamera::color_space::TransferFunction::Srgb) => {
            if full {
                ColorSpace::Srgb
            } else {
                ColorSpace::Bt709
            }
        }
        _ => {
            if full {
                ColorSpace::Srgb
            } else {
                ColorSpace::Unknown
            }
        }
    }
}

#[cfg(feature = "probe")]
fn convert_value(val: &LcValue) -> ControlValue {
    match val {
        LcValue::None => ControlValue::None,
        LcValue::Bool(v) => v
            .first()
            .copied()
            .map(ControlValue::Bool)
            .unwrap_or(ControlValue::None),
        LcValue::Byte(v) => v
            .first()
            .copied()
            .map(|b| ControlValue::Uint(b as u32))
            .unwrap_or(ControlValue::None),
        LcValue::Uint16(v) => v
            .first()
            .copied()
            .map(|b| ControlValue::Uint(b as u32))
            .unwrap_or(ControlValue::None),
        LcValue::Uint32(v) => v
            .first()
            .copied()
            .map(ControlValue::Uint)
            .unwrap_or(ControlValue::None),
        LcValue::Int32(v) => v
            .first()
            .copied()
            .map(ControlValue::Int)
            .unwrap_or(ControlValue::None),
        LcValue::Int64(v) => v
            .first()
            .copied()
            .map(|i| ControlValue::Int(i.clamp(i32::MIN as i64, i32::MAX as i64) as i32))
            .unwrap_or(ControlValue::None),
        LcValue::Float(v) => v
            .first()
            .copied()
            .map(ControlValue::Float)
            .unwrap_or(ControlValue::None),
        _ => ControlValue::None,
    }
}

/// Placeholder libcamera capture source.
pub struct LibcameraCapture {
    descriptor: CaptureDescriptor,
}

impl LibcameraCapture {
    /// Create a new libcamera capture source with the provided descriptor.
    pub fn new(descriptor: CaptureDescriptor) -> Self {
        Self { descriptor }
    }
}

impl CaptureSource for LibcameraCapture {
    fn descriptor(&self) -> &CaptureDescriptor {
        &self.descriptor
    }

    fn next_frame(&self) -> Option<FrameLease> {
        // Stub: real implementation would poll libcamera streams.
        None
    }
}

pub mod prelude {
    #[cfg(feature = "probe")]
    pub use crate::{
        DEFAULT_LIBCAMERA_PROBE_CACHE_MS, LibcameraManagerConfig, manager_config, probe_devices,
        probe_devices_uncached, probe_devices_uncached_with_errors, probe_devices_with_errors,
        set_manager_config, subscribe_hotplug_events,
    };
    pub use crate::{LibcameraCapture, LibcameraDeviceInfo};
    pub use styx_capture::prelude::*;
}

#![doc = include_str!("../README.md")]
use smallvec::smallvec;
use std::num::NonZeroU32;
use std::panic::catch_unwind;
use styx_capture::prelude::*;
use styx_core::controls::{Access, ControlKind, ControlMetadata, ControlValue};
use v4l::format::Colorspace as V4lColorspace;
use v4l::{capability::Flags, framesize::FrameSizeEnum, prelude::*, video::Capture};

fn read_node_name(path: &std::path::Path) -> Option<String> {
    let node = path.file_name()?.to_string_lossy();
    let sysfs = format!("/sys/class/video4linux/{node}/name");
    std::fs::read_to_string(sysfs)
        .ok()
        .map(|s| s.trim().to_string())
}

/// V4L2 device information with a descriptor built from advertised formats.
pub struct V4l2DeviceInfo {
    pub path: String,
    pub name: Option<String>,
    pub card: String,
    pub driver: String,
    pub bus_info: String,
    pub properties: Vec<(String, String)>,
    pub descriptor: CaptureDescriptor,
}

/// Probe devices and return (devices, errors) for observability.
pub fn probe_devices() -> (Vec<V4l2DeviceInfo>, Vec<String>) {
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    for dev in v4l::context::enum_devices() {
        match build_info(dev.path()) {
            Ok(info) => devices.push(info),
            Err(e) => errors.push(format!("{}: {e}", dev.path().display())),
        };
    }
    (devices, errors)
}

fn build_info(path: &std::path::Path) -> Result<V4l2DeviceInfo, Box<dyn std::error::Error>> {
    let dev = Device::with_path(path)?;
    let caps = dev.query_caps()?;
    let node_name = read_node_name(path);

    if !(caps.capabilities.contains(Flags::VIDEO_CAPTURE)
        || caps.capabilities.contains(Flags::VIDEO_CAPTURE_MPLANE))
    {
        // Skip non-capture nodes (e.g., decoders/encoders) to avoid probing controls they expose.
        return Err("not a capture device".into());
    }
    let card = caps.card;
    let driver = caps.driver;
    let bus_info = caps.bus;
    let driver_lc = driver.to_ascii_lowercase();
    let card_lc = card.to_ascii_lowercase();

    // Skip pipeline-internal nodes we don't want to expose as cameras.
    let name_lc = node_name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if card_lc.contains("virtual")
        || driver_lc.contains("virtual")
        || driver_lc.contains("pispbe")
        || card_lc.contains("pispbe")
        || card_lc.contains("pisp")
        || driver_lc.contains("rp1-cfe")
        || card_lc.contains("rp1-cfe")
        || name_lc.contains("rp1-cfe")
        || name_lc.contains("embedded")
        || name_lc.contains("config")
        || name_lc.contains("fe_")
        || name_lc.contains("stats")
    {
        return Err("filtered non-camera node".into());
    }

    // Be tolerant of quirky drivers: if formats or frame sizes fail, keep probing
    // whatever we can instead of dropping the device entirely.
    let mut modes = Vec::new();
    let default_color = dev
        .format()
        .ok()
        .map(|fmt| map_color_space(Some(fmt.colorspace)))
        .unwrap_or(ColorSpace::Unknown);
    let formats = dev.enum_formats().unwrap_or_default();
    for fmt in formats {
        let fourcc = FourCc::from(u32::from_le_bytes(fmt.fourcc.repr));
        let color = if default_color != ColorSpace::Unknown {
            default_color
        } else {
            guess_color_space(fourcc)
        };
        let framesizes = match dev.enum_framesizes(fmt.fourcc) {
            Ok(sizes) => sizes,
            Err(_) => continue,
        };
        // Pick discrete sizes; stepwise could be supported later.
        for size in framesizes {
            match size.size {
                FrameSizeEnum::Discrete(fs) => {
                    if let Some(res) = Resolution::new(fs.width, fs.height) {
                        let mut intervals = smallvec![];
                        let ivals = dev
                            .enum_frameintervals(fmt.fourcc, fs.width, fs.height)
                            .unwrap_or_default();
                        for iv in ivals {
                            if let v4l::frameinterval::FrameIntervalEnum::Discrete(discrete) =
                                iv.interval
                                && let (Some(n), Some(d)) = (
                                    NonZeroU32::new(discrete.numerator),
                                    NonZeroU32::new(discrete.denominator),
                                )
                            {
                                intervals.push(Interval {
                                    numerator: n,
                                    denominator: d,
                                });
                            }
                        }
                        let format = MediaFormat::new(fourcc, res, color);
                        modes.push(Mode {
                            id: ModeId {
                                format,
                                interval: None,
                            },
                            format,
                            intervals,
                            interval_stepwise: None,
                        });
                    }
                }
                FrameSizeEnum::Stepwise(step) => {
                    if let Some(res) = Resolution::new(step.min_width, step.min_height) {
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
    }

    // Some kernels expose controls with newer/unknown types; the upstream v4l crate
    // currently panics when converting those descriptions. Catch the panic and simply
    // drop the controls list so that device discovery can still succeed.
    let controls = match catch_unwind(|| dev.query_controls()) {
        Ok(Ok(ctrls)) => ctrls
            .into_iter()
            .filter_map(|ctrl| map_control(ctrl).ok())
            .collect::<Vec<_>>(),
        // If controls cannot be queried (ENOTTY) or the v4l crate hit an unsupported
        // control type, ignore controls rather than skipping the device entirely.
        Ok(Err(_)) | Err(_) => Vec::new(),
    };

    let descriptor = CaptureDescriptor { modes, controls };
    Ok(V4l2DeviceInfo {
        path: path.display().to_string(),
        name: node_name.clone(),
        card: card.clone(),
        driver: driver.clone(),
        bus_info: bus_info.clone(),
        properties: vec![
            ("path".into(), path.display().to_string()),
            ("name".into(), node_name.unwrap_or_default()),
            ("driver".into(), driver),
            ("card".into(), card),
            ("bus".into(), bus_info),
        ],
        descriptor,
    })
}

fn map_control(ctrl: v4l::control::Description) -> Result<ControlMeta, Box<dyn std::error::Error>> {
    let id = ControlId(ctrl.id);
    let name = ctrl.name;
    use v4l::control::Type::*;
    let (min, max, default) = match ctrl.typ {
        Integer => (
            ControlValue::Int(ctrl.minimum as i32),
            ControlValue::Int(ctrl.maximum as i32),
            ControlValue::Int(ctrl.default as i32),
        ),
        Boolean => (
            ControlValue::Bool(ctrl.minimum != 0),
            ControlValue::Bool(ctrl.maximum != 0),
            ControlValue::Bool(ctrl.default != 0),
        ),
        Menu | IntegerMenu => (
            ControlValue::Uint(ctrl.minimum as u32),
            ControlValue::Uint(ctrl.maximum as u32),
            ControlValue::Uint(ctrl.default as u32),
        ),
        Bitmask | Integer64 | CtrlClass | Button | String => {
            return Err("unsupported control type".into());
        }
        _ => {
            return Err("unsupported control type".into());
        }
    };

    let access = if ctrl.flags.contains(v4l::control::Flags::READ_ONLY) {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };

    let menu = ctrl.items.map(|items| {
        items
            .into_iter()
            .map(|(_, item)| item.to_string())
            .collect()
    });

    let step = match ctrl.typ {
        Integer | Integer64 | Bitmask | IntegerMenu => Some(ControlValue::Uint(ctrl.step as u32)),
        _ => None,
    };

    Ok(ControlMeta {
        id,
        name,
        kind: match ctrl.typ {
            Integer => ControlKind::Int,
            Boolean => ControlKind::Bool,
            Menu => ControlKind::Menu,
            IntegerMenu => ControlKind::IntMenu,
            Bitmask => ControlKind::Uint,
            Integer64 => ControlKind::Int,
            CtrlClass | Button | String => ControlKind::Unknown,
            _ => ControlKind::Unknown,
        },
        access,
        min,
        max,
        default,
        step,
        menu,
        metadata: ControlMetadata::default(),
    })
}

fn map_color_space(cs: Option<V4lColorspace>) -> ColorSpace {
    match cs {
        Some(V4lColorspace::SRGB) => ColorSpace::Srgb,
        Some(V4lColorspace::Rec709) => ColorSpace::Bt709,
        Some(V4lColorspace::Rec2020) => ColorSpace::Bt2020,
        Some(V4lColorspace::SMPTE170M) => ColorSpace::Bt709,
        _ => ColorSpace::Unknown,
    }
}

fn guess_color_space(fcc: FourCc) -> ColorSpace {
    match &fcc.to_u32().to_le_bytes() {
        b"MJPG" | b"JPEG" | b"RG24" | b"RGB3" | b"RGB6" | b"BG24" | b"RGBA" | b"BGRA" | b"XB24"
        | b"XR24" => ColorSpace::Srgb,
        b"NV12" | b"NV21" | b"NV16" | b"NV61" | b"NV24" | b"NV42" | b"YUYV" | b"YVYU" | b"UYVY"
        | b"VYUY" | b"I420" | b"YU12" | b"YV12" | b"YU16" | b"YV16" | b"YU24" | b"YV24" => {
            ColorSpace::Bt709
        }
        _ => ColorSpace::Unknown,
    }
}

pub mod prelude {
    pub use crate::{V4l2DeviceInfo, probe_devices};
    pub use styx_capture::prelude::*;
}

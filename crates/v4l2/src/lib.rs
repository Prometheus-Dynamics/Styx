#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]
use smallvec::smallvec;
use std::num::NonZeroU32;
use styx_capture::prelude::*;
use styx_core::controls::{Access, ControlKind, ControlMetadata, ControlValue};
use styx_kernel::v4l2::{
    BufType, CapabilityFlags, ControlFlags, ControlInfo, ControlType, Controls, Format,
    FrameIntervals, FrameSizes, MenuValue, VideoDevice,
};

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
    for path in styx_kernel::v4l2::list_video_nodes() {
        match build_info(&path) {
            Ok(info) => devices.push(info),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        };
    }
    (devices, errors)
}

fn build_info(path: &std::path::Path) -> Result<V4l2DeviceInfo, Box<dyn std::error::Error>> {
    let dev = VideoDevice::open(path)?;
    let caps = dev.capabilities().clone();
    let node_name = read_node_name(path);

    if !(caps.device_caps.contains(CapabilityFlags::VIDEO_CAPTURE)
        || caps
            .device_caps
            .contains(CapabilityFlags::VIDEO_CAPTURE_MPLANE))
    {
        // Skip non-capture nodes (e.g., decoders/encoders) to avoid probing controls they expose.
        return Err("not a capture device".into());
    }
    let card = caps.card;
    let driver = caps.driver;
    let bus_info = caps.bus_info;
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
    let default_color = match dev.format(BufType::VideoCapture) {
        Ok(Format::Single(pix)) => map_color_space(pix.colorspace),
        _ => ColorSpace::Unknown,
    };
    let modes = probe_modes(&dev, default_color);

    // If controls cannot be queried (ENOTTY), ignore them rather than skipping the device.
    // Controls of types Styx does not model are left out one by one.
    let controls = match dev.query_controls() {
        Ok(ctrls) => ctrls
            .into_iter()
            .filter_map(|ctrl| map_control(&dev, ctrl))
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
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

/// One mode per advertised format and discrete size, with its discrete frame intervals.
fn probe_modes(dev: &VideoDevice, default_color: ColorSpace) -> Vec<Mode> {
    let mut modes = Vec::new();
    let formats = dev.formats(BufType::VideoCapture).unwrap_or_default();
    for fmt in formats {
        let fourcc = FourCc::from(fmt.fourcc.to_u32());
        let color = if default_color != ColorSpace::Unknown {
            default_color
        } else {
            guess_color_space(fourcc)
        };
        let Ok(framesizes) = dev.frame_sizes(fmt.fourcc) else {
            continue;
        };
        let mode = |res: Resolution, intervals| {
            let format = MediaFormat::new(fourcc, res, color);
            Mode {
                id: ModeId {
                    format,
                    interval: None,
                },
                format,
                intervals,
                interval_stepwise: None,
            }
        };
        // Advertise concrete modes only. Stepwise frame-size ranges are not
        // expanded because doing so would invent modes the driver did not list.
        match framesizes {
            FrameSizes::Discrete(sizes) => {
                for fs in sizes {
                    let Some(res) = Resolution::new(fs.width, fs.height) else {
                        continue;
                    };
                    let mut intervals = smallvec![];
                    if let Ok(FrameIntervals::Discrete(ivals)) =
                        dev.frame_intervals(fmt.fourcc, fs.width, fs.height)
                    {
                        for iv in ivals {
                            if let (Some(n), Some(d)) = (
                                NonZeroU32::new(iv.numerator),
                                NonZeroU32::new(iv.denominator),
                            ) {
                                intervals.push(Interval {
                                    numerator: n,
                                    denominator: d,
                                });
                            }
                        }
                    }
                    modes.push(mode(res, intervals));
                }
            }
            FrameSizes::Stepwise(step) | FrameSizes::Continuous(step) => {
                if let Some(res) = Resolution::new(step.min_width, step.min_height) {
                    modes.push(mode(res, smallvec![]));
                }
            }
        }
    }
    modes
}

/// Describes a control for Styx; `None` for types Styx does not model (64-bit, bitmask,
/// button, string, class markers, compound).
fn map_control(dev: &VideoDevice, ctrl: ControlInfo) -> Option<ControlMeta> {
    let (min, max, default, kind) = match ctrl.control_type {
        ControlType::Integer => (
            ControlValue::Int(ctrl.minimum as i32),
            ControlValue::Int(ctrl.maximum as i32),
            ControlValue::Int(ctrl.default as i32),
            ControlKind::Int,
        ),
        ControlType::Boolean => (
            ControlValue::Bool(ctrl.minimum != 0),
            ControlValue::Bool(ctrl.maximum != 0),
            ControlValue::Bool(ctrl.default != 0),
            ControlKind::Bool,
        ),
        ControlType::Menu | ControlType::IntegerMenu => (
            ControlValue::Uint(ctrl.minimum as u32),
            ControlValue::Uint(ctrl.maximum as u32),
            ControlValue::Uint(ctrl.default as u32),
            if ctrl.control_type == ControlType::Menu {
                ControlKind::Menu
            } else {
                ControlKind::IntMenu
            },
        ),
        _ => return None,
    };

    let access = if ctrl.flags.contains(ControlFlags::READ_ONLY) {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };

    // Menu items the driver skips (VIDIOC_QUERYMENU fails) are left out.
    let menu = ctrl.is_menu().then(|| {
        dev.query_menu(&ctrl)
            .unwrap_or_default()
            .into_iter()
            .map(|item| match item.value {
                MenuValue::Name(name) => name,
                MenuValue::Integer(value) => value.to_string(),
            })
            .collect()
    });

    let step = match ctrl.control_type {
        ControlType::Integer | ControlType::IntegerMenu => {
            Some(ControlValue::Uint(ctrl.step as u32))
        }
        _ => None,
    };

    Some(ControlMeta {
        id: ControlId(ctrl.id),
        name: ctrl.name,
        kind,
        access,
        min,
        max,
        default,
        step,
        menu,
        metadata: ControlMetadata::default(),
    })
}

/// Maps `enum v4l2_colorspace`.
fn map_color_space(colorspace: u32) -> ColorSpace {
    const SMPTE170M: u32 = 1;
    const REC709: u32 = 3;
    const SRGB: u32 = 8;
    const BT2020: u32 = 10;
    match colorspace {
        SRGB => ColorSpace::Srgb,
        REC709 | SMPTE170M => ColorSpace::Bt709,
        BT2020 => ColorSpace::Bt2020,
        _ => ColorSpace::Unknown,
    }
}

fn guess_color_space(fcc: FourCc) -> ColorSpace {
    fcc.info()
        .default_color_space
        .unwrap_or(ColorSpace::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorspaces_map_like_the_kernel_enum() {
        assert_eq!(map_color_space(8), ColorSpace::Srgb);
        assert_eq!(map_color_space(3), ColorSpace::Bt709);
        assert_eq!(map_color_space(1), ColorSpace::Bt709);
        assert_eq!(map_color_space(10), ColorSpace::Bt2020);
        assert_eq!(map_color_space(0), ColorSpace::Unknown);
        assert_eq!(map_color_space(7), ColorSpace::Unknown);
    }
}

pub mod prelude {
    pub use crate::{V4l2DeviceInfo, probe_devices};
    pub use styx_capture::prelude::*;
}

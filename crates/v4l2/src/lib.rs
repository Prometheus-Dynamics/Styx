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

/// Whether virtual capture nodes (v4l2loopback, "OBS Virtual Camera", ...) are kept: set
/// `STYX_V4L2_ALLOW_VIRTUAL=1`. They are skipped by default because they are usually fed by
/// another application rather than being cameras; they are useful for testing.
fn allow_virtual() -> bool {
    std::env::var_os("STYX_V4L2_ALLOW_VIRTUAL").is_some_and(|v| !v.is_empty() && v != "0")
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
///
/// Nodes that are not cameras (decoders, ISP nodes, a receiver's embedded data, statistics
/// and configuration nodes, UVC metadata nodes, virtual cameras) and nodes that disappeared
/// while probing are skipped quietly (logged at debug level); `errors` holds real failures
/// only (a node that could not be opened or queried).
pub fn probe_devices() -> (Vec<V4l2DeviceInfo>, Vec<String>) {
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    for path in styx_kernel::v4l2::list_video_nodes() {
        match build_info(&path) {
            Ok(Probed::Camera(info)) => devices.push(info),
            Ok(Probed::Skipped(why)) => {
                tracing::debug!(node = %path.display(), "v4l2 probe: skipped, {why}");
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        };
    }
    (devices, errors)
}

/// What a video node turned out to be.
enum Probed {
    Camera(V4l2DeviceInfo),
    /// Not a camera, or gone: why.
    Skipped(String),
}

/// Why a node that opened is not a camera Styx offers, if it is not.
fn skip_reason(
    device_caps: CapabilityFlags,
    driver: &str,
    card: &str,
    node_name: Option<&str>,
    allow_virtual: bool,
) -> Option<&'static str> {
    if !(device_caps.contains(CapabilityFlags::VIDEO_CAPTURE)
        || device_caps.contains(CapabilityFlags::VIDEO_CAPTURE_MPLANE))
    {
        // Decoders/encoders, output and metadata nodes: not probed for controls either.
        return Some("not a capture node");
    }
    let driver_lc = driver.to_ascii_lowercase();
    let card_lc = card.to_ascii_lowercase();
    let name_lc = node_name.unwrap_or_default().to_ascii_lowercase();
    if (card_lc.contains("virtual") || driver_lc.contains("virtual")) && !allow_virtual {
        return Some("virtual camera (set STYX_V4L2_ALLOW_VIRTUAL=1 to keep it)");
    }
    // Pipeline-internal nodes: a receiver or ISP node needs the camera's pipeline (native or
    // libcamera) to produce anything.
    if driver_lc.contains("pispbe")
        || card_lc.contains("pisp")
        || driver_lc.contains("rp1-cfe")
        || card_lc.contains("rp1-cfe")
        || name_lc.contains("rp1-cfe")
        || name_lc.contains("embedded")
        || name_lc.contains("config")
        || name_lc.contains("fe_")
        || name_lc.contains("stats")
    {
        return Some("camera pipeline node");
    }
    None
}

fn build_info(path: &std::path::Path) -> Result<Probed, Box<dyn std::error::Error>> {
    let dev = match VideoDevice::open(path) {
        Ok(dev) => dev,
        // Unplugged between listing and opening.
        Err(e) if matches!(e.errno(), Some(libc::ENOENT | libc::ENODEV | libc::ENXIO)) => {
            return Ok(Probed::Skipped(format!("gone ({e})")));
        }
        Err(e) => return Err(e.into()),
    };
    let caps = dev.capabilities().clone();
    let node_name = read_node_name(path);
    if let Some(why) = skip_reason(
        caps.device_caps,
        &caps.driver,
        &caps.card,
        node_name.as_deref(),
        allow_virtual(),
    ) {
        return Ok(Probed::Skipped(format!(
            "{why}: {} ({}, {})",
            node_name.as_deref().unwrap_or(&caps.card),
            caps.driver,
            caps.bus_info
        )));
    }
    let card = caps.card;
    let driver = caps.driver;
    let bus_info = caps.bus_info;

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
    Ok(Probed::Camera(V4l2DeviceInfo {
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
    }))
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

    #[test]
    fn non_camera_nodes_are_skipped_not_reported() {
        let cap = CapabilityFlags::VIDEO_CAPTURE | CapabilityFlags::STREAMING;
        let skip =
            |caps, driver, card, name: &str| skip_reason(caps, driver, card, Some(name), false);
        // The CM5's nodes (Raspberry Pi 6.12): receiver, front end, back end, HEVC decoder.
        for (caps, driver, card, name) in [
            (cap, "rp1-cfe", "rp1-cfe", "rp1-cfe-csi2_ch0"),
            (cap, "rp1-cfe", "rp1-cfe", "rp1-cfe-embedded"),
            (
                CapabilityFlags::META_CAPTURE,
                "rp1-cfe",
                "rp1-cfe",
                "rp1-cfe-fe_stats",
            ),
            (
                CapabilityFlags::META_OUTPUT,
                "rp1-cfe",
                "rp1-cfe",
                "rp1-cfe-fe_config",
            ),
            (
                CapabilityFlags::VIDEO_OUTPUT,
                "pispbe",
                "PiSP Back End",
                "pispbe-input",
            ),
            (cap, "pispbe", "PiSP Back End", "pispbe-output0"),
            (
                CapabilityFlags::VIDEO_M2M_MPLANE,
                "rpi-hevc-dec",
                "rpi-hevc-dec",
                "rpi-hevc-dec",
            ),
            // A UVC camera's metadata node.
            (
                CapabilityFlags::META_CAPTURE,
                "uvcvideo",
                "UVC Camera (046d:0825)",
                "UVC Camera (046d:0825)",
            ),
            (
                cap,
                "v4l2 loopback",
                "OBS Virtual Camera",
                "OBS Virtual Camera",
            ),
        ] {
            assert!(
                skip(caps, driver, card, name).is_some(),
                "{name} is not a camera"
            );
        }
        assert_eq!(
            skip(
                cap,
                "uvcvideo",
                "UVC Camera (046d:0825)",
                "UVC Camera (046d:0825)"
            ),
            None
        );
        assert_eq!(
            skip_reason(cap, "v4l2 loopback", "OBS Virtual Camera", None, true),
            None,
            "kept when asked for"
        );
    }
}

pub mod prelude {
    pub use crate::{V4l2DeviceInfo, probe_devices};
    pub use styx_capture::prelude::*;
}

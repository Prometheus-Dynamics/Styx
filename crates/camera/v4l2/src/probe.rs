//! Probing: which capture nodes exist, and what each one offers.

use std::path::{Path, PathBuf};

use styx_controls::Control;
use styx_format::{FourCc, Interval};
use styx_lemnos::v4l2::v4l2::{
    BufType, CapabilityFlags, ControlFlags, ControlType, Controls, FrameIntervals, FrameSizes,
    VideoDevice, list_video_nodes,
};

use crate::Error;
use crate::controls::Table;

/// Every capture node on this machine, and the nodes that failed to answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub devices: Vec<Device>,
    /// `path: why` for each node that could not be probed. Nodes that are not capture nodes
    /// are left out without a line here.
    pub errors: Vec<String>,
}

/// Probes every video node. Nodes that are not capture nodes are skipped quietly.
pub fn probe() -> Probe {
    let mut out = Probe::default();
    for path in list_video_nodes() {
        match probe_node(&path) {
            Ok(Some(device)) => out.devices.push(device),
            Ok(None) => {}
            Err(e) => out.errors.push(format!("{}: {e}", path.display())),
        }
    }
    out
}

/// Probes one node: `None` when it is not a capture node.
pub fn probe_node(path: &Path) -> Result<Option<Device>, Error> {
    let dev = VideoDevice::open_read_only(path)?;
    let Some((queue, buf_type)) = queue_of(&dev) else {
        return Ok(None);
    };
    let mut errors = Vec::new();
    let caps = dev.capabilities().clone();
    let formats = match dev.formats(buf_type) {
        Ok(list) => list
            .into_iter()
            .map(|desc| {
                let code = styx_code(desc.fourcc.to_bytes());
                let sizes = match dev.frame_sizes(desc.fourcc) {
                    Ok(sizes) => sizes_of(&dev, desc.fourcc, sizes),
                    Err(e) => {
                        errors.push(format!("{code}: frame sizes: {e}"));
                        Sizes::Discrete(Vec::new())
                    }
                };
                Format {
                    code,
                    description: desc.description,
                    sizes,
                }
            })
            .collect(),
        Err(e) => {
            errors.push(format!("formats: {e}"));
            Vec::new()
        }
    };
    let table = Table::build(&dev, buf_type);
    let raw_controls = match dev.query_controls() {
        Ok(infos) => infos
            .iter()
            .map(|info| RawControl {
                id: info.id,
                name: info.name.clone(),
                control_type: info.control_type,
                min: info.minimum,
                max: info.maximum,
                default: info.default,
                read_only: info.flags.contains(ControlFlags::READ_ONLY),
                mapped: mapped_control(info.id),
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let virtual_node =
        caps.driver.contains("loopback") || caps.card.to_ascii_lowercase().contains("virtual");
    Ok(Some(Device {
        path: path.to_path_buf(),
        card: caps.card,
        driver: caps.driver,
        bus_info: caps.bus_info,
        queue,
        formats,
        controls: table.supported(),
        raw_controls,
        virtual_node,
        errors,
    }))
}

/// A capture node, as probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: PathBuf,
    pub card: String,
    pub driver: String,
    pub bus_info: String,
    pub queue: Queue,
    pub formats: Vec<Format>,
    /// The Styx controls this node maps, in [`Control::ALL`] order.
    pub controls: Vec<Control>,
    /// Every control the node reports, mapped or not.
    pub raw_controls: Vec<RawControl>,
    /// A virtual node (a loopback device, a virtual camera): fed by another program, not a
    /// sensor. Reported, not skipped.
    pub virtual_node: bool,
    /// Parts of the probe that failed; the rest is still here.
    pub errors: Vec<String>,
}

/// Which capture queue the node uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// `V4L2_BUF_TYPE_VIDEO_CAPTURE`.
    Single,
    /// `V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE`.
    Multi,
}

/// A pixel format the node offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub code: FourCc,
    pub description: String,
    pub sizes: Sizes,
}

/// The frame sizes a format offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sizes {
    Discrete(Vec<Size>),
    Stepwise(SizeRange),
    Continuous(SizeRange),
}

/// One listed frame size and its intervals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
    pub intervals: Intervals,
}

/// A range of sizes, with the intervals at its smallest size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeRange {
    pub min: (u32, u32),
    pub max: (u32, u32),
    pub step: (u32, u32),
    pub intervals_at_min: Intervals,
}

/// Frame intervals (seconds per frame) a size offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intervals {
    Discrete(Vec<Interval>),
    Stepwise {
        min: Interval,
        max: Interval,
        step: Interval,
    },
    Continuous {
        min: Interval,
        max: Interval,
    },
    /// The driver did not say.
    Unknown,
}

/// A control the node reports, as the driver describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawControl {
    pub id: u32,
    pub name: String,
    pub control_type: ControlType,
    pub min: i64,
    pub max: i64,
    pub default: i64,
    pub read_only: bool,
    /// The Styx control this one maps to, when this crate maps it.
    pub mapped: Option<Control>,
}

/// The capture queue of a node: the single-planar one if it has it, else the multi-planar.
pub(crate) fn queue_of(dev: &VideoDevice) -> Option<(Queue, BufType)> {
    let caps = dev.capabilities().device_caps;
    if caps.contains(CapabilityFlags::VIDEO_CAPTURE) {
        Some((Queue::Single, BufType::VideoCapture))
    } else if caps.contains(CapabilityFlags::VIDEO_CAPTURE_MPLANE) {
        Some((Queue::Multi, BufType::VideoCaptureMplane))
    } else {
        None
    }
}

fn styx_code(bytes: [u8; 4]) -> FourCc {
    FourCc::new(bytes)
}

fn sizes_of(dev: &VideoDevice, fourcc: styx_lemnos::v4l2::FourCc, sizes: FrameSizes) -> Sizes {
    let range = |r: styx_lemnos::v4l2::v4l2::StepwiseSize| SizeRange {
        min: (r.min_width, r.min_height),
        max: (r.max_width, r.max_height),
        step: (r.step_width, r.step_height),
        intervals_at_min: intervals_of(dev, fourcc, r.min_width, r.min_height),
    };
    match sizes {
        FrameSizes::Discrete(list) => Sizes::Discrete(
            list.into_iter()
                .map(|s| Size {
                    width: s.width,
                    height: s.height,
                    intervals: intervals_of(dev, fourcc, s.width, s.height),
                })
                .collect(),
        ),
        FrameSizes::Stepwise(r) => Sizes::Stepwise(range(r)),
        FrameSizes::Continuous(r) => Sizes::Continuous(range(r)),
    }
}

fn intervals_of(
    dev: &VideoDevice,
    fourcc: styx_lemnos::v4l2::FourCc,
    width: u32,
    height: u32,
) -> Intervals {
    match dev.frame_intervals(fourcc, width, height) {
        Ok(FrameIntervals::Discrete(list)) => {
            Intervals::Discrete(list.into_iter().filter_map(interval_opt).collect())
        }
        Ok(FrameIntervals::Stepwise { min, max, step }) => {
            match (interval_opt(min), interval_opt(max), interval_opt(step)) {
                (Some(min), Some(max), Some(step)) => Intervals::Stepwise { min, max, step },
                _ => Intervals::Unknown,
            }
        }
        Ok(FrameIntervals::Continuous { min, max }) => match (interval_opt(min), interval_opt(max))
        {
            (Some(min), Some(max)) => Intervals::Continuous { min, max },
            _ => Intervals::Unknown,
        },
        Err(_) => Intervals::Unknown,
    }
}

/// A driver's interval as a format-crate interval; a zero numerator or denominator is no interval.
fn interval_opt(f: styx_lemnos::v4l2::Fraction) -> Option<Interval> {
    Interval::new(f.numerator, f.denominator)
}

fn mapped_control(id: u32) -> Option<Control> {
    Control::ALL.into_iter().find(|&control| match control {
        Control::Exposure => id == styx_lemnos::v4l2::v4l2::cid::EXPOSURE_ABSOLUTE,
        Control::WhiteBalance => id == styx_lemnos::v4l2::v4l2::cid::WHITE_BALANCE_TEMPERATURE,
        Control::Focus => id == styx_lemnos::v4l2::v4l2::cid::FOCUS_ABSOLUTE,
        Control::HFlip => id == styx_lemnos::v4l2::v4l2::cid::HFLIP,
        Control::VFlip => id == styx_lemnos::v4l2::v4l2::cid::VFLIP,
        Control::FrameRate | Control::Gain | Control::Crop => false,
    })
}

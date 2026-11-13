//! Probing: which video nodes exist (from sysfs, opening none), and what one node offers (a full
//! probe, which opens that node once).
//!
//! Listing never opens a node. Opening a node can start a virtual camera's output, wake a camera or
//! take a device that allows one open at a time, so a node is opened only when a camera is brought
//! up ([`crate::Stream::start`]) or when a caller names it for a full probe ([`probe_node`],
//! [`probe_card`], [`probe_input`]).

use std::fs;
use std::path::{Path, PathBuf};

use styx_controls::{Control, DeviceControl, DeviceKind, DeviceName};
use styx_format::{FourCc, Interval};
use styx_lemnos::v4l2::v4l2::{
    BufType, CapabilityFlags, ControlFlags, ControlInfo, ControlType, Controls, FrameIntervals,
    FrameSizes, MenuValue, VideoDevice,
};

use crate::Error;
use crate::controls::{Table, control_of};

/// The sysfs class directory of video nodes.
pub const SYSFS_CLASS: &str = "/sys/class/video4linux";
/// The device directory the nodes are named in.
pub const DEV_DIR: &str = "/dev";

/// Every video node the sysfs class lists. Nothing is opened to list them: a node's role (capture or
/// output) is not in sysfs, so it stays unknown until the node is probed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub nodes: Vec<Node>,
}

/// A video node as sysfs describes it, without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The `/dev` path of the node.
    pub path: PathBuf,
    /// The sysfs `name` (the card name for most drivers, for example `vivid-000-vid-cap`).
    pub name: String,
    /// The sysfs `index`: the node's index within its device.
    pub index: Option<u32>,
    /// The bound driver's name (the basename of the `device/driver` link), when bound.
    pub driver: Option<String>,
    /// The canonical sysfs device directory (the `device` link): the bus or parent device.
    pub device_path: Option<PathBuf>,
    /// A virtual node: its device lives under `/sys/devices/virtual`, or a loopback driver feeds it.
    /// Another program produces its frames. Listed, never opened by listing.
    pub virtual_node: bool,
    /// Which queue the node uses. `None`: sysfs does not say, and it is known only once the node is
    /// probed.
    pub queue: Option<Queue>,
    /// Whether this listing opened the node. Always `false`: listing opens nothing.
    pub opened: bool,
}

/// Opens a node. The production opener opens the node; a test opener records the calls.
pub trait Opener {
    /// Opens `path`: read-only for a full probe, read-write when an input must be selected.
    fn open(&self, path: &Path, write: bool) -> Result<VideoDevice, Error>;
}

/// The production opener: the node itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Opener for System {
    fn open(&self, path: &Path, write: bool) -> Result<VideoDevice, Error> {
        if write {
            Ok(VideoDevice::open(path)?)
        } else {
            Ok(VideoDevice::open_read_only(path)?)
        }
    }
}

/// Lists and probes video nodes through an [`Opener`], from a sysfs class directory.
#[derive(Debug)]
pub struct Prober<O: Opener> {
    opener: O,
    sysfs: PathBuf,
    dev: PathBuf,
}

impl Prober<System> {
    /// The machine's video nodes: `/sys/class/video4linux`, nodes under `/dev`.
    pub fn system() -> Self {
        Self::new(System, SYSFS_CLASS, DEV_DIR)
    }
}

impl<O: Opener> Prober<O> {
    /// A prober over `sysfs` (a video class directory) and `dev` (the directory its nodes are in).
    pub fn new(opener: O, sysfs: impl Into<PathBuf>, dev: impl Into<PathBuf>) -> Self {
        Self {
            opener,
            sysfs: sysfs.into(),
            dev: dev.into(),
        }
    }

    /// Lists the nodes from sysfs. Opens nothing: the opener is not called.
    pub fn list(&self) -> Listing {
        let mut names: Vec<String> = fs::read_dir(&self.sysfs)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort_by_key(|name| natural_key(name));
        Listing {
            nodes: names
                .into_iter()
                .map(|name| {
                    let info = SysInfo::read(&self.sysfs.join(&name));
                    Node {
                        path: self.dev.join(&name),
                        name: info.name,
                        index: info.index,
                        driver: info.driver,
                        device_path: info.device_path,
                        virtual_node: info.virtual_node,
                        queue: None,
                        opened: false,
                    }
                })
                .collect(),
        }
    }

    /// Fully probes one node: its formats, sizes, intervals and controls. `None` when the node is
    /// not a capture node. Opens the node once, read-only.
    pub fn probe_node(&self, path: &Path) -> Result<Option<Device>, Error> {
        let dev = self.opener.open(path, false)?;
        let sys = SysInfo::read(&self.sysfs_dir_of(path));
        describe(&dev, path, &sys, None)
    }

    /// Fully probes one node after selecting input `input` on it. Opens the node once, read-write
    /// (an input is selected by `VIDIOC_S_INPUT`, which a read-only handle may not do). `None` when
    /// the node is not a capture node.
    pub fn probe_input(&self, path: &Path, input: u32) -> Result<Option<Device>, Error> {
        let dev = self.opener.open(path, true)?;
        dev.set_input(input)?;
        let sys = SysInfo::read(&self.sysfs_dir_of(path));
        describe(&dev, path, &sys, Some(input))
    }

    /// Fully probes the one node whose sysfs name is `card` (matched by name, never by number).
    /// `Ok(None)` when no node has that name or the node is not a capture node; an error when more
    /// than one node has it.
    pub fn probe_card(&self, card: &str) -> Result<Option<Device>, Error> {
        let listing = self.list();
        let mut named = listing.nodes.iter().filter(|node| node.name == card);
        let Some(node) = named.next() else {
            return Ok(None);
        };
        if named.next().is_some() {
            return Err(Error::Unsupported(format!(
                "more than one video node is named {card:?}; name the node by path"
            )));
        }
        self.probe_node(&node.path)
    }

    fn sysfs_dir_of(&self, path: &Path) -> PathBuf {
        self.sysfs.join(path.file_name().unwrap_or_default())
    }
}

/// Lists the machine's video nodes from sysfs. Opens nothing.
pub fn probe() -> Listing {
    Prober::system().list()
}

/// Fully probes one node by path: `None` when it is not a capture node. Opens that node once.
pub fn probe_node(path: &Path) -> Result<Option<Device>, Error> {
    Prober::system().probe_node(path)
}

/// Fully probes the one node named `card` in sysfs. Opens that node once.
pub fn probe_card(card: &str) -> Result<Option<Device>, Error> {
    Prober::system().probe_card(card)
}

/// Fully probes one node after selecting `input` on it. Opens that node once, read-write.
pub fn probe_input(path: &Path, input: u32) -> Result<Option<Device>, Error> {
    Prober::system().probe_input(path, input)
}

/// What sysfs says about one node, read without opening it.
#[derive(Debug, Default)]
struct SysInfo {
    name: String,
    index: Option<u32>,
    driver: Option<String>,
    device_path: Option<PathBuf>,
    virtual_node: bool,
}

impl SysInfo {
    fn read(dir: &Path) -> SysInfo {
        let name = read_trimmed(&dir.join("name")).unwrap_or_default();
        let index = read_trimmed(&dir.join("index")).and_then(|s| s.parse().ok());
        let driver = fs::read_link(dir.join("device/driver"))
            .ok()
            .and_then(|link| link.file_name().map(|n| n.to_string_lossy().into_owned()));
        let device_path = fs::canonicalize(dir.join("device")).ok();
        // A virtual node may have no device link at all (OBS's loopback camera has none): its class
        // directory itself lives under /sys/devices/virtual.
        let class_path = fs::canonicalize(dir).ok();
        let virtual_node = is_virtual(
            class_path.as_deref(),
            device_path.as_deref(),
            driver.as_deref(),
        );
        SysInfo {
            name,
            index,
            driver,
            device_path,
            virtual_node,
        }
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// A node is virtual when its class directory or its device lives under `/sys/devices/virtual`, or a
/// loopback driver feeds it.
fn is_virtual(class_path: Option<&Path>, device_path: Option<&Path>, driver: Option<&str>) -> bool {
    let under_virtual = |p: Option<&Path>| p.is_some_and(|p| p.starts_with("/sys/devices/virtual"));
    under_virtual(class_path)
        || under_virtual(device_path)
        || driver.is_some_and(|d| d.to_ascii_lowercase().contains("loopback"))
}

/// Sort key: the name's text, then its trailing number (`video2` before `video10`).
fn natural_key(name: &str) -> (String, u32) {
    let digits = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let number = name[digits.len()..].parse().unwrap_or(0);
    (digits.to_string(), number)
}

/// Describes an open node: `None` when it has no capture queue.
fn describe(
    dev: &VideoDevice,
    path: &Path,
    sys: &SysInfo,
    input: Option<u32>,
) -> Result<Option<Device>, Error> {
    let Some((queue, buf_type)) = queue_of(dev) else {
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
                    Ok(sizes) => sizes_of(dev, desc.fourcc, sizes),
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
    let infos = dev.query_controls().unwrap_or_default();
    let table = Table::from_infos(dev, &infos, buf_type);
    let raw_controls = infos
        .iter()
        .map(|info| RawControl::from_info(dev, info))
        .collect::<Vec<_>>();
    let virtual_node = sys.virtual_node
        || is_virtual(None, None, Some(&caps.driver))
        || caps.card.to_ascii_lowercase().contains("virtual");
    Ok(Some(Device {
        path: path.to_path_buf(),
        card: caps.card,
        driver: caps.driver,
        bus_info: caps.bus_info,
        queue,
        input,
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
    /// The input this node was probed on (see [`probe_input`]); `None` for the driver's own.
    pub input: Option<u32>,
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

/// A control the node reports, as the driver describes it. Every control a node offers is listed,
/// mapped or not: a control with no Styx control is a passthrough, read and set by its `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawControl {
    pub id: u32,
    pub name: String,
    pub control_type: ControlType,
    pub min: i64,
    pub max: i64,
    /// The step between valid values (0 when the driver does not say).
    pub step: u64,
    pub default: i64,
    pub read_only: bool,
    /// The items of a menu control, in index order; empty for other types.
    pub menu: Vec<DeviceMenuItem>,
    /// The Styx control this one maps to, when this crate maps it.
    pub mapped: Option<Control>,
}

/// One item of a menu control: the value that selects it and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMenuItem {
    pub value: i64,
    pub name: String,
}

impl RawControl {
    pub(crate) fn from_info(dev: &VideoDevice, info: &ControlInfo) -> RawControl {
        let menu = if info.is_menu() {
            dev.query_menu(info)
                .unwrap_or_default()
                .into_iter()
                .map(|item| DeviceMenuItem {
                    value: i64::from(item.index),
                    name: match item.value {
                        MenuValue::Name(name) => name,
                        MenuValue::Integer(value) => value.to_string(),
                    },
                })
                .collect()
        } else {
            Vec::new()
        };
        RawControl {
            id: info.id,
            name: info.name.clone(),
            control_type: info.control_type,
            min: info.minimum,
            max: info.maximum,
            step: info.step,
            default: info.default,
            read_only: info.flags.contains(ControlFlags::READ_ONLY),
            menu,
            mapped: control_of(info.id),
        }
    }

    /// The control as a [`DeviceControl`]: its id, name, kind and bounds, for a passthrough.
    pub fn device_control(&self) -> DeviceControl {
        DeviceControl {
            id: self.id,
            name: DeviceName::new(&self.name),
            kind: match self.control_type {
                ControlType::Integer => DeviceKind::Integer,
                ControlType::Integer64 => DeviceKind::Integer64,
                ControlType::Boolean => DeviceKind::Boolean,
                ControlType::Menu | ControlType::IntegerMenu => DeviceKind::Menu,
                ControlType::Bitmask => DeviceKind::Bitmask,
                ControlType::Button => DeviceKind::Button,
                _ => DeviceKind::Other,
            },
            min: self.min,
            max: self.max,
            step: self.step,
            default: self.default,
            read_only: self.read_only,
        }
    }
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

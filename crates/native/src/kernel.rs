//! Sensors with an upstream kernel driver: found through the media graph (entities with
//! function `MEDIA_ENT_F_CAM_SENSOR`), described from what their subdevice reports (codes,
//! sizes, crop, `PIXEL_RATE`, `HBLANK`, `VBLANK`, `EXPOSURE`, `ANALOGUE_GAIN`, flips, test
//! patterns) plus an optional data file ([`KernelSensorData`]: gain model, delays, black level,
//! embedded data), and driven through V4L2 controls by the same driver, control schedule and
//! session as a bridged sensor.
//!
//! ```text
//! open        the subdevice (exclusive between Styx processes: an advisory lock; busy while
//!             libcamera has the media device locked), flips to the description's defaults
//! configure   sensor format; the driver's ranges for that mode re-read and the description
//!             rebuilt from them; mode defaults and the start values as controls
//! start       start values set (at once: not streaming yet), then the receiver's STREAMON
//!             starts the sensor (s_stream), then the control schedule runs;
//!             frame starts (FRAME_SYNC) drive the schedule: each control is set `delay`
//!             frames before the frame it is for, VBLANK in a call of its own first
//! stop        STREAMOFF stops the sensor; the driver powers it down (runtime PM)
//! ```
//!
//! A sensor that also has a bridge (the same I²C address) is left to the bridge.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use styx_kernel::bus::{BridgeLocation, find_bridges};
use styx_kernel::media::{self, EntityFunction, MediaDevice, Topology};
use styx_kernel::subdev::{Subdev, Which};
use styx_kernel::v4l2::{ControlFlags, ControlWhich, Controls, MenuValue, SelectionTarget};
use styx_sensor::{
    ControlRange, EmbeddedFormat, EmbeddedPacking, KernelControl, KernelSensorData, MbusCode,
    NoPins, Rect, SensorDescription, SensorDriver, Size, SubdevFormat, SubdevReport,
};

use crate::camera::{CameraOptions, NativeCamera, StreamSettings, select_mode};
use crate::control::{SensorControl, lock};
use crate::discover::{CameraInfo, query_raw_formats};
use crate::error::{KernelContext, NativeError, Result};
use crate::library::SensorLibrary;
use crate::modes::{SensorMode, sensor_modes};
use crate::sensor_bus::{CameraPins, KernelBridge, SensorBus, SubdevBus};
use crate::session::{BufferSource, Session, SessionOptions};
use crate::stream::SensorSide;
use crate::topology::find_route;

/// What a kernel driver's sensor reported, and the data that completed its description.
#[derive(Clone, Debug)]
pub struct KernelSensor {
    /// The media entity's name, e.g. `ov9782 10-0060`.
    pub entity: String,
    /// What the subdevice reported (at discovery, then at the last configuration).
    pub report: SubdevReport,
    /// The data file that applied, if any.
    pub data: Option<KernelSensorData>,
    /// Where it came from (a path, or `builtin:<name>`).
    pub data_source: Option<String>,
    /// The sensor's device tree node, as libcamera names cameras
    /// (`/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60`).
    pub firmware_node: Option<String>,
}

/// The key of a kernel driver's sensor.
pub fn key_for(subdev: &Path) -> String {
    format!("sensor:{}", subdev.display())
}

/// The I²C bus and address in a sensor entity's name (`ov9782 10-0060`).
pub fn i2c_of(entity: &str) -> (Option<u32>, Option<u16>) {
    let Some((bus, addr)) = entity
        .split_whitespace()
        .nth(1)
        .and_then(|a| a.split_once('-'))
    else {
        return (None, None);
    };
    (bus.parse().ok(), u16::from_str_radix(addr, 16).ok())
}

const KERNEL_CONTROLS: [KernelControl; 10] = [
    KernelControl::Exposure,
    KernelControl::AnalogueGain,
    KernelControl::DigitalGain,
    KernelControl::Vblank,
    KernelControl::Hblank,
    KernelControl::PixelRate,
    KernelControl::LinkFreq,
    KernelControl::HFlip,
    KernelControl::VFlip,
    KernelControl::TestPattern,
];

/// Reads what a sensor subdevice reports on its source pad `pad`.
pub fn read_report(sd: &Subdev, pad: u32, entity: &str) -> Result<SubdevReport> {
    let mut report = SubdevReport {
        name: entity.to_owned(),
        ..Default::default()
    };
    for code in sd
        .mbus_codes(pad, Which::Active)
        .step("enumerate the sensor's codes")?
    {
        let mut sizes: Vec<Size> = sd
            .frame_sizes(pad, code, Which::Active)
            .step("enumerate the sensor's sizes")?
            .into_iter()
            .map(|r| Size::new(r.max_width, r.max_height))
            .collect();
        sizes.dedup();
        report.formats.push(SubdevFormat {
            code: MbusCode(code.0),
            sizes,
        });
    }
    let rect = |t| {
        sd.selection(pad, Which::Active, t).ok().map(|r| Rect {
            left: u32::try_from(r.left).unwrap_or(0),
            top: u32::try_from(r.top).unwrap_or(0),
            width: r.width,
            height: r.height,
        })
    };
    report.native_size = rect(SelectionTarget::NativeSize).map(|r| Size::new(r.width, r.height));
    report.crop_bounds = rect(SelectionTarget::CropBounds);
    report.current_size = sd
        .format(pad, Which::Active)
        .ok()
        .map(|f| Size::new(f.width, f.height));
    for c in KERNEL_CONTROLS {
        let Ok(info) = sd.query_control(c.cid()) else {
            continue;
        };
        let value = sd
            .get_controls_for(ControlWhich::Current, std::slice::from_ref(&info))
            .ok()
            .and_then(|v| v.first().and_then(|v| v.as_i64()));
        report.controls.insert(
            c,
            ControlRange {
                min: info.minimum,
                max: info.maximum,
                step: info.step,
                default: info.default,
                value,
            },
        );
        let items = || sd.query_menu(&info).unwrap_or_default();
        match c {
            KernelControl::LinkFreq => {
                report.link_frequencies = items()
                    .into_iter()
                    .filter_map(|i| match i.value {
                        MenuValue::Integer(v) => Some(v),
                        MenuValue::Name(_) => None,
                    })
                    .collect();
            }
            KernelControl::TestPattern => {
                report.test_patterns = items()
                    .into_iter()
                    .filter_map(|i| match i.value {
                        MenuValue::Name(n) => Some((i.index, n)),
                        MenuValue::Integer(_) => None,
                    })
                    .collect();
            }
            KernelControl::HFlip | KernelControl::VFlip => {
                report.flips_modify_layout |= info.flags.contains(ControlFlags::MODIFY_LAYOUT);
            }
            _ => {}
        }
    }
    Ok(report)
}

/// The description of a kernel driver's sensor: [`SensorDescription::from_subdev_with`],
/// with a CCS embedded data layout's padding set for the bit depth of `code` (default: the
/// first code reported).
pub fn describe(
    report: &SubdevReport,
    data: Option<&KernelSensorData>,
    code: Option<u32>,
) -> Result<SensorDescription> {
    let mut desc = SensorDescription::from_subdev_with(report, data)?;
    let bits = code
        .map(MbusCode)
        .or(report.formats.first().map(|f| f.code))
        .and_then(MbusCode::bit_depth);
    if let Some(e) = desc.embedded_data.as_mut()
        && e.format == EmbeddedFormat::Ccs
    {
        e.packing = match bits {
            Some(10) => EmbeddedPacking::Raw10,
            Some(12) => EmbeddedPacking::Raw12,
            _ => EmbeddedPacking::None,
        };
    }
    Ok(desc)
}

/// The sensor's device tree node path under `/sys/firmware/devicetree`, as libcamera names
/// cameras.
fn firmware_node(subdev: &Path) -> Option<String> {
    let name = subdev.file_name()?.to_str()?;
    let node =
        std::fs::canonicalize(format!("/sys/class/video4linux/{name}/device/of_node")).ok()?;
    node.strip_prefix("/sys/firmware/devicetree")
        .ok()
        .map(|p| format!("/{}", p.display()).replace("//", "/"))
}

/// Whether a sensor entity is (or has) a Styx sensor bridge.
fn bridged(entity: &str, subdev: &Path, bridges: &[BridgeLocation]) -> bool {
    let i2c = i2c_of(entity);
    entity.contains("styx-sensor-bridge")
        || bridges.iter().any(|b| {
            b.subdev == subdev
                || (i2c.0.is_some() && i2c.1.is_some() && (b.i2c_bus, b.i2c_address) == i2c)
        })
}

/// Sensor entities with a kernel driver in one media graph: `(entity id, name, subdev)`.
fn sensors_in(topo: &Topology, bridges: &[BridgeLocation]) -> Vec<(u32, String, PathBuf)> {
    topo.entities
        .iter()
        .filter(|e| e.function == EntityFunction::CAM_SENSOR)
        .filter_map(|e| Some((e.id, e.name.clone(), topo.devnode_path(e.id)?)))
        .filter(|(_, name, path)| !bridged(name, path, bridges))
        .collect()
}

/// Everything about one kernel driver's sensor.
fn discover_sensor(
    media: &Path,
    topology: &Topology,
    entity: u32,
    name: &str,
    subdev: &Path,
    library: &SensorLibrary,
) -> Result<CameraInfo> {
    let route = find_route(topology, entity)?;
    let sd = Subdev::open_read_only(subdev).step("open the sensor subdev")?;
    let report = read_report(&sd, route.sensor_pad, name)?;
    let colour = report
        .formats
        .first()
        .and_then(|f| f.code.color_filter())
        .is_some_and(|c| c != styx_sensor::ColorFilter::Mono);
    let found = library.find_kernel_data(name, colour);
    let (data, data_source) = found.map_or((None, None), |(d, s)| (Some(d), Some(s)));
    let description = describe(&report, data.as_ref(), None)?;
    let (i2c_bus, i2c_address) = i2c_of(name);
    let location = BridgeLocation {
        subdev: subdev.to_path_buf(),
        sensor_name: description.sensor.name.clone(),
        i2c_bus,
        i2c_address,
        clock_frequency: 0,
    };
    let modes = sensor_modes(&description);
    let raw_formats = query_raw_formats(&route, &modes);
    let description_source = match &data_source {
        Some(s) => format!("kernel driver + {s}"),
        None => "kernel driver (generic defaults)".to_owned(),
    };
    let mut info = CameraInfo {
        key: key_for(subdev),
        location,
        description: Arc::new(description),
        description_source,
        modes,
        media: media.to_path_buf(),
        topology: topology.clone(),
        route,
        raw_formats,
        graph: crate::graph::GraphMap::empty(),
        kernel: Some(KernelSensor {
            entity: name.to_owned(),
            report,
            data,
            data_source,
            firmware_node: firmware_node(subdev),
        }),
    };
    info.rebuild_graph(topology.clone());
    Ok(info)
}

/// Every sensor with a kernel driver (and no bridge) on this system, and the problems met on
/// the way (a driver without the controls the timing model needs, a graph of an unexpected
/// shape).
pub fn discover_kernel(library: &SensorLibrary) -> (Vec<CameraInfo>, Vec<NativeError>) {
    let bridges = find_bridges().unwrap_or_default();
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for path in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open_read_only(&path) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        for (id, name, subdev) in sensors_in(&topo, &bridges) {
            match discover_sensor(&path, &topo, id, &name, &subdev, library) {
                Ok(info) => found.push(info),
                Err(e) => errors.push(NativeError::InvalidConfig(format!("\"{name}\": {e}"))),
            }
        }
    }
    (found, errors)
}

/// The keys of the kernel driver's sensors bound now, with their media device and entity.
pub fn kernel_keys() -> Vec<String> {
    let bridges = find_bridges().unwrap_or_default();
    media::list_media_devices()
        .into_iter()
        .filter_map(|p| MediaDevice::open_read_only(&p).ok()?.topology().ok())
        .flat_map(|t| sensors_in(&t, &bridges))
        .map(|(_, _, subdev)| key_for(&subdev))
        .collect()
}

/// The kernel driver's sensor with this key.
pub fn discover_key(key: &str, library: &SensorLibrary) -> Result<CameraInfo> {
    let bridges = find_bridges().unwrap_or_default();
    for path in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open_read_only(&path) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        for (id, name, subdev) in sensors_in(&topo, &bridges) {
            if key_for(&subdev) == key {
                return discover_sensor(&path, &topo, id, &name, &subdev, library);
            }
        }
    }
    Err(NativeError::Topology(format!("no sensor {key}")))
}

impl NativeCamera {
    /// [`NativeCamera::open`] for a kernel driver's sensor (`lock`: the advisory lock taken on
    /// the subdevice).
    pub(crate) fn open_kernel(
        info: CameraInfo,
        options: CameraOptions,
        opened: Instant,
        lock: File,
    ) -> Result<Self> {
        // libcamera locks the media device while it has one of its cameras acquired.
        if let Ok(media) = MediaDevice::open_read_only(&info.media)
            && let Ok(Some(pid)) = media.lock_holder()
        {
            return Err(NativeError::Busy(format!(
                "{} is locked by process {pid} (libcamera has a camera of it)",
                info.media.display()
            )));
        }
        let subdev = Arc::new(Subdev::open(&info.location.subdev).step("open the sensor subdev")?);
        // Flips to the description's defaults before anything sets a format: drivers that do
        // not flag the flips as changing the layout report codes that hold with them off.
        let mut bus = SubdevBus::new(Arc::clone(&subdev));
        let desc = Arc::clone(&info.description);
        let flips: Vec<(KernelControl, i64)> = [
            (desc.controls.hflip, KernelControl::HFlip),
            (desc.controls.vflip, KernelControl::VFlip),
        ]
        .into_iter()
        .filter_map(|(f, c)| f.map(|f| (c, i64::from(f.default))))
        .collect();
        if !flips.is_empty() {
            styx_sensor::RegisterBus::set_controls(&mut bus, &flips).step("set the flips")?;
        }
        let driver = SensorDriver::new(desc, SensorBus::Kernel(bus), CameraPins::None(NoPins));
        let control = Arc::new(Mutex::new(SensorControl::new(driver)));
        let sensor: Arc<dyn SensorSide> = control.clone();
        let bridge = Arc::new(KernelBridge::new().step("kernel sensor events")?);
        let session = Session::new(
            bridge,
            sensor,
            SessionOptions {
                buffers: options.buffers,
                source: BufferSource::Memory(options.memory.clone()),
                max_error_frames: options.max_error_frames,
            },
        );
        Ok(Self::assemble(
            info,
            options,
            None,
            Some(subdev),
            control,
            session,
            opened,
            lock,
        ))
    }

    /// The sensor part of `configure` for a kernel driver's sensor: the sensor format, the
    /// description rebuilt from the driver's ranges for that mode, the mode's defaults and the
    /// start values. Returns the mode and its frame length.
    pub(crate) fn configure_kernel_sensor(
        &mut self,
        settings: &StreamSettings,
    ) -> Result<(SensorMode, u32)> {
        let sd = self
            .kernel
            .clone()
            .ok_or(NativeError::State("not a kernel driver's sensor"))?;
        let ks = self
            .info
            .kernel
            .clone()
            .ok_or(NativeError::State("not a kernel driver's sensor"))?;
        // The rate is checked against the mode's own ranges, known once it is set (until then
        // every mode has the ranges of the mode that was set at discovery).
        let any_rate = StreamSettings {
            interval: None,
            ..*settings
        };
        let chosen = select_mode(&self.info.modes, &any_rate, &self.info.raw_formats)?.clone();
        let pad = self.info.route.sensor_pad;
        let mut f = sd.format(pad, Which::Active).step("sensor format")?;
        f.width = chosen.width;
        f.height = chosen.height;
        f.code = styx_kernel::subdev::MbusCode(chosen.code);
        let got = sd
            .set_format(pad, Which::Active, &f)
            .step("set the sensor format")?;
        if (got.width, got.height, got.code.0) != (chosen.width, chosen.height, chosen.code) {
            return Err(NativeError::InvalidConfig(format!(
                "the sensor chose {}x{} code {:#x} for {}x{} code {:#x}",
                got.width, got.height, got.code.0, chosen.width, chosen.height, chosen.code
            )));
        }
        // The driver's ranges (pixel rate, blanking, exposure) are those of this mode now.
        let report = read_report(&sd, pad, &ks.entity)?;
        let desc = Arc::new(describe(&report, ks.data.as_ref(), Some(chosen.code))?);
        let modes = sensor_modes(&desc);
        let mode = modes
            .iter()
            .find(|m| m.mode == chosen.mode && m.format == chosen.format)
            .cloned()
            .ok_or(NativeError::State("the mode vanished from the driver"))?;
        if let Some(i) = settings.interval
            && !mode.allows(i)
        {
            return Err(NativeError::InvalidConfig(format!(
                "{:.3} fps is outside {} {}'s {:.3}..{:.3} fps",
                i.fps(),
                mode.mode,
                mode.format,
                mode.min_fps(),
                mode.max_fps()
            )));
        }
        self.info.description = Arc::clone(&desc);
        self.info.modes = modes;
        if let Some(k) = self.info.kernel.as_mut() {
            k.report = report;
        }
        let topology = self.info.topology.clone();
        self.info.rebuild_graph(topology);
        let frame_length = {
            let mut c = lock(&self.control);
            c.driver_mut().set_description(desc)?;
            c.bring_up(&mode.mode, &mode.format)?;
            let fl = crate::camera::start_frame_length(&mut c, settings)?;
            // Set now: the driver applies what is set when the receiver starts it, and with
            // another device's capture the schedule starts only after that.
            c.driver_mut().issue_now()?;
            fl
        };
        Ok((mode, frame_length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_names_give_the_i2c_address() {
        assert_eq!(i2c_of("ov9782 10-0060"), (Some(10), Some(0x60)));
        assert_eq!(i2c_of("imx708_wide 4-001a"), (Some(4), Some(0x1a)));
        assert_eq!(i2c_of("mock-sensor"), (None, None));
        assert_eq!(
            key_for(Path::new("/dev/v4l-subdev2")),
            "sensor:/dev/v4l-subdev2"
        );
    }

    #[test]
    fn sensors_with_a_bridge_are_left_to_it() {
        let bridge = BridgeLocation {
            subdev: "/dev/v4l-subdev2".into(),
            sensor_name: "ov9782".into(),
            i2c_bus: Some(10),
            i2c_address: Some(0x60),
            clock_frequency: 0,
        };
        let p = Path::new("/dev/v4l-subdev5");
        assert!(bridged("ov9782 10-0060", p, std::slice::from_ref(&bridge)));
        assert!(bridged("ov9782 styx-sensor-bridge-cam0", p, &[]));
        assert!(!bridged("imx219 11-0010", p, std::slice::from_ref(&bridge)));
        assert!(!bridged("ov9782 10-0060", p, &[]));
    }

    #[test]
    fn ccs_padding_follows_the_bit_depth() {
        let report = SubdevReport {
            name: "imx477 10-001a".into(),
            formats: vec![
                SubdevFormat {
                    code: MbusCode(0x3012),
                    sizes: vec![Size::new(2028, 1520)],
                },
                SubdevFormat {
                    code: MbusCode(0x300f),
                    sizes: vec![Size::new(1332, 990)],
                },
            ],
            controls: [
                (
                    KernelControl::PixelRate,
                    (840_000_000, 840_000_000, 840_000_000),
                ),
                (KernelControl::Exposure, (4, 65487, 1000)),
                (KernelControl::AnalogueGain, (0, 978, 0)),
                (KernelControl::Vblank, (58, 65000, 2000)),
                (KernelControl::Hblank, (9000, 9000, 9000)),
            ]
            .into_iter()
            .map(|(c, (min, max, default))| {
                (
                    c,
                    ControlRange {
                        min,
                        max,
                        step: 1,
                        default,
                        value: Some(default),
                    },
                )
            })
            .collect(),
            ..Default::default()
        };
        let all = KernelSensorData::builtin();
        let data = KernelSensorData::find(&all, &report.name, true).unwrap();
        let d12 = describe(&report, Some(data), None).unwrap();
        let e = d12.embedded_data.as_ref().unwrap();
        assert_eq!(
            (e.format, e.packing),
            (EmbeddedFormat::Ccs, EmbeddedPacking::Raw12)
        );
        let d10 = describe(&report, Some(data), Some(0x300f)).unwrap();
        assert_eq!(d10.embedded_data.unwrap().packing, EmbeddedPacking::Raw10);
        // 1024 / (1024 - 512) = 2x.
        assert_eq!(d12.controls.analog_gain.gain_for_code(512), 2.0);
        assert_eq!(d12.controls.delays.frame_length, 3);
    }
}

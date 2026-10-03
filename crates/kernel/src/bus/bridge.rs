//! Userspace side of the Styx sensor bridge (`kernel-modules/styx-sensor-bridge/PROTOCOL.md`).
//!
//! The bridge module is the sensor subdevice the CSI-2 receiver sees. A userspace sensor driver
//! opens its subdev node with [`SensorBridge::open`], publishes the pad format and timing
//! ([`SensorBridge::set_format`], [`SensorBridge::set_timing`]), [`subscribes`] to stream
//! requests and answers each [`StreamRequest`] with [`SensorBridge::acknowledge`] once the sensor
//! has started (after the receiver is ready) or stopped.
//!
//! `VIDIOC_STREAMON` on the receiver's video node blocks until the start is acknowledged, so the
//! acknowledging code must run on another thread (or process, or an async task on a reactor
//! that is not blocked by the STREAMON call).
//!
//! [`subscribes`]: SensorBridge::subscribe

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::bridge_sys::*;
use crate::Error;
use crate::event::{EventKind, EventType, Events, SubscribeFlags};
use crate::subdev::{MbusCode, MbusFormat, Subdev, Which};
use crate::v4l2::{ControlValue, ControlWhich, Controls, MenuValue, cid};

/// The bridge's stream request event.
const STREAM_EVENT: EventType = EventType::Private(EVENT_STREAM - V4L2_EVENT_PRIVATE_START);

/// Driver name the bridge module registers.
pub const DRIVER_NAME: &str = "styx-sensor-bridge";

/// What the receiver asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamAction {
    /// The receiver is ready: start the sensor's output now.
    Start,
    /// The receiver is stopping: stop the sensor's output now.
    Stop,
}

/// Bridge stream state (`STYX_CID_STREAM_STATE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Not streaming.
    Idle,
    /// A start request is waiting for acknowledgement.
    Starting,
    /// Streaming.
    Streaming,
    /// A stop request is waiting for acknowledgement.
    Stopping,
    /// A start failed but the receiver was told it succeeded (the bridge's default, see
    /// `PROTOCOL.md`): stop the receiver to get back to idle.
    StartFailed,
}

impl StreamState {
    fn from_raw(v: i32) -> io::Result<Self> {
        Ok(match v {
            0 => Self::Idle,
            1 => Self::Starting,
            2 => Self::Streaming,
            3 => Self::Stopping,
            4 => Self::StartFailed,
            _ => return Err(io::Error::other(format!("unknown bridge stream state {v}"))),
        })
    }
}

/// A stream start/stop request from the bridge, with the format and timing in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRequest {
    /// Start or stop.
    pub action: StreamAction,
    /// Echoed in the acknowledgement.
    pub sequence: u32,
    /// How long the bridge waits for the acknowledgement.
    pub timeout: Duration,
    /// Link frequency in Hz.
    pub link_freq: i64,
    /// Pixel rate in pixels per second.
    pub pixel_rate: i64,
    /// Media bus code.
    pub code: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in lines.
    pub height: u32,
    /// Horizontal blanking in pixels.
    pub hblank: i32,
    /// Vertical blanking in lines.
    pub vblank: i32,
    /// CSI-2 data lanes.
    pub data_lanes: u32,
    /// Continuous (not gated) CSI-2 clock.
    pub continuous_clock: bool,
}

fn u32_at(d: &[u8; 64], off: usize) -> u32 {
    u32::from_ne_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn i64_at(d: &[u8; 64], off: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[off..off + 8]);
    i64::from_ne_bytes(b)
}

impl StreamRequest {
    /// Decodes `struct styx_bridge_stream_event` from a `v4l2_event` payload.
    pub fn decode(data: &[u8; 64]) -> io::Result<Self> {
        let version = u32_at(data, 0);
        if version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "bridge protocol version {version}, expected {PROTOCOL_VERSION}"
            )));
        }
        let action = match u32_at(data, 4) {
            ACTION_START => StreamAction::Start,
            ACTION_STOP => StreamAction::Stop,
            a => return Err(io::Error::other(format!("unknown bridge action {a}"))),
        };
        Ok(Self {
            action,
            sequence: u32_at(data, 8),
            timeout: Duration::from_millis(u64::from(u32_at(data, 12))),
            link_freq: i64_at(data, 16),
            pixel_rate: i64_at(data, 24),
            code: u32_at(data, 32),
            width: u32_at(data, 36),
            height: u32_at(data, 40),
            hblank: u32_at(data, 44) as i32,
            vblank: u32_at(data, 48) as i32,
            data_lanes: u32_at(data, 52),
            continuous_clock: u32_at(data, 56) & FLAG_CONTINUOUS_CLOCK != 0,
        })
    }

    /// Encodes the request as the kernel would (for tests and simulations).
    pub fn encode(&self) -> [u8; 64] {
        let mut d = [0u8; 64];
        let mut put = |off: usize, bytes: &[u8]| d[off..off + bytes.len()].copy_from_slice(bytes);
        put(0, &PROTOCOL_VERSION.to_ne_bytes());
        let action = match self.action {
            StreamAction::Start => ACTION_START,
            StreamAction::Stop => ACTION_STOP,
        };
        put(4, &action.to_ne_bytes());
        put(8, &self.sequence.to_ne_bytes());
        put(12, &(self.timeout.as_millis() as u32).to_ne_bytes());
        put(16, &self.link_freq.to_ne_bytes());
        put(24, &self.pixel_rate.to_ne_bytes());
        put(32, &self.code.to_ne_bytes());
        put(36, &self.width.to_ne_bytes());
        put(40, &self.height.to_ne_bytes());
        put(44, &self.hblank.to_ne_bytes());
        put(48, &self.vblank.to_ne_bytes());
        put(52, &self.data_lanes.to_ne_bytes());
        put(
            56,
            &(if self.continuous_clock {
                FLAG_CONTINUOUS_CLOCK
            } else {
                0
            })
            .to_ne_bytes(),
        );
        d
    }
}

/// The acknowledgement control value: sequence in the low half, errno (0 = done) in the high half.
pub fn ack_control_value(sequence: u32, result: Result<(), i32>) -> i64 {
    let status = match result {
        Ok(()) => 0,
        Err(errno) => errno.unsigned_abs().clamp(1, 4095),
    };
    ack_value(sequence, status)
}

/// Sensor timing the receiver and Styx read from the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Index into the device tree's `link-frequencies` ([`SensorBridge::link_frequencies`]).
    pub link_freq_index: u32,
    /// Pixels per second.
    pub pixel_rate: i64,
    /// Horizontal blanking in pixels (line length − width).
    pub hblank: i32,
    /// Vertical blanking in lines (frame length − height).
    pub vblank: i32,
}

/// The source pad format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadFormat {
    /// Media bus code.
    pub code: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// A bridge found in sysfs, with what its device tree node says about the sensor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeLocation {
    /// Subdev node, e.g. `/dev/v4l-subdev2`.
    pub subdev: PathBuf,
    /// `styx,sensor-name`.
    pub sensor_name: String,
    /// I²C adapter number of `styx,i2c-bus`, if given.
    pub i2c_bus: Option<u32>,
    /// `styx,i2c-address`, if given.
    pub i2c_address: Option<u16>,
    /// Sensor input clock in Hz (0 if the node names no clock).
    pub clock_frequency: u64,
}

fn read_attr(dir: &Path, name: &str) -> io::Result<String> {
    Ok(std::fs::read_to_string(dir.join(name))?.trim().to_owned())
}

/// Parses the bridge's sysfs attributes (pure, given their text).
pub(crate) fn parse_location(
    subdev: PathBuf,
    name: &str,
    bus: &str,
    addr: &str,
    clock: &str,
) -> BridgeLocation {
    let i2c_bus = bus.parse::<i64>().ok().and_then(|b| u32::try_from(b).ok());
    let i2c_address = u16::from_str_radix(addr.trim_start_matches("0x"), 16)
        .ok()
        .filter(|a| *a != 0);
    BridgeLocation {
        subdev,
        sensor_name: name.to_owned(),
        i2c_bus,
        i2c_address,
        clock_frequency: clock.parse().unwrap_or(0),
    }
}

/// Lists the bridges bound on this system (empty if none, or no V4L2 at all).
pub fn find_bridges() -> io::Result<Vec<BridgeLocation>> {
    let class = Path::new("/sys/class/video4linux");
    let Ok(entries) = std::fs::read_dir(class) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let node = entry.file_name().to_string_lossy().into_owned();
        if !node.starts_with("v4l-subdev") {
            continue;
        }
        let dev = entry.path().join("device");
        let is_bridge = std::fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n == DRIVER_NAME))
            .unwrap_or(false);
        if !is_bridge {
            continue;
        }
        found.push(parse_location(
            PathBuf::from("/dev").join(&node),
            &read_attr(&dev, "sensor_name")?,
            &read_attr(&dev, "i2c_bus")?,
            &read_attr(&dev, "i2c_address")?,
            &read_attr(&dev, "clock_frequency")?,
        ));
    }
    found.sort_by(|a, b| a.subdev.cmp(&b.subdev));
    Ok(found)
}

/// An open bridge subdevice.
#[derive(Debug)]
pub struct SensorBridge {
    subdev: Subdev,
    link_frequencies: Vec<i64>,
}

/// Converts a kernel-layer error to the `io::Error` of its errno (so callers can match
/// `raw_os_error`, e.g. `ESTALE` from a late acknowledgement).
fn io_error(err: Error) -> io::Error {
    match err.errno() {
        Some(errno) => io::Error::from_raw_os_error(errno),
        None => io::Error::other(err),
    }
}

impl SensorBridge {
    /// Opens a bridge subdev node (non-blocking; events are waited for with `poll`).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let subdev = Subdev::open(path).map_err(|e| match e {
            Error::Open { source, .. } => source,
            other => io_error(other),
        })?;
        let mut bridge = Self {
            subdev,
            link_frequencies: Vec::new(),
        };
        bridge.link_frequencies = bridge.query_int_menu(cid::LINK_FREQ)?;
        Ok(bridge)
    }

    /// Node path.
    pub fn path(&self) -> &Path {
        self.subdev.path()
    }

    /// The link frequencies from the device tree, in Hz, indexed as [`Timing::link_freq_index`].
    pub fn link_frequencies(&self) -> &[i64] {
        &self.link_frequencies
    }

    /// The items of an integer menu, by index; every index must be present.
    fn query_int_menu(&self, id: u32) -> io::Result<Vec<i64>> {
        let info = self.subdev.query_control(id).map_err(io_error)?;
        let items = self.subdev.query_menu(&info).map_err(io_error)?;
        let first = info.minimum.max(0);
        let expected = (info.maximum - first + 1).max(0) as usize;
        let complete = items.len() == expected
            && items
                .iter()
                .enumerate()
                .all(|(i, item)| i64::from(item.index) == first + i as i64);
        if !complete {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        items
            .into_iter()
            .map(|item| match item.value {
                MenuValue::Integer(v) => Ok(v),
                MenuValue::Name(_) => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            })
            .collect()
    }

    /// Sets the active source pad format. The bridge clamps to its limits and replaces a code it
    /// does not accept; the result is what it chose. Fails with `EBUSY` while streaming.
    pub fn set_format(&self, format: PadFormat) -> io::Result<PadFormat> {
        let f = MbusFormat {
            width: format.width,
            height: format.height,
            code: MbusCode(format.code),
            field: 1, // V4L2_FIELD_NONE
            ..Default::default()
        };
        let f = self
            .subdev
            .set_format(0, Which::Active, &f)
            .map_err(io_error)?;
        Ok(PadFormat {
            code: f.code.0,
            width: f.width,
            height: f.height,
        })
    }

    /// The active source pad format.
    pub fn format(&self) -> io::Result<PadFormat> {
        let f = self.subdev.format(0, Which::Active).map_err(io_error)?;
        Ok(PadFormat {
            code: f.code.0,
            width: f.width,
            height: f.height,
        })
    }

    fn set_controls(&self, values: &[(u32, ControlValue)]) -> io::Result<()> {
        self.subdev
            .set_controls(ControlWhich::Current, values)
            .map_err(io_error)
    }

    fn set_i32(&self, id: u32, value: i32) -> io::Result<()> {
        self.set_controls(&[(id, ControlValue::Integer(value))])
    }

    fn get_i32(&self, id: u32) -> io::Result<i32> {
        match self.subdev.control(id).map_err(io_error)? {
            ControlValue::Integer(v) => Ok(v),
            other => Err(io::Error::other(format!(
                "control {id:#x}: unexpected value {other:?}"
            ))),
        }
    }

    /// Sets link frequency, pixel rate and blanking in one atomic control write. Link frequency
    /// and pixel rate cannot change while streaming; blanking can (frame rate changes).
    pub fn set_timing(&self, timing: Timing) -> io::Result<()> {
        if timing.link_freq_index as usize >= self.link_frequencies.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "link frequency index out of range",
            ));
        }
        self.set_controls(&[
            (
                cid::LINK_FREQ,
                ControlValue::Integer(timing.link_freq_index as i32),
            ),
            (cid::PIXEL_RATE, ControlValue::Integer64(timing.pixel_rate)),
            (cid::HBLANK, ControlValue::Integer(timing.hblank)),
            (cid::VBLANK, ControlValue::Integer(timing.vblank)),
        ])
    }

    /// Sets horizontal and vertical blanking only.
    pub fn set_blanking(&self, hblank: i32, vblank: i32) -> io::Result<()> {
        self.set_controls(&[
            (cid::HBLANK, ControlValue::Integer(hblank)),
            (cid::VBLANK, ControlValue::Integer(vblank)),
        ])
    }

    /// Enables or disables the supplies and clock the device tree gives the bridge. Powering off
    /// fails with `EBUSY` while streaming.
    pub fn set_power(&self, on: bool) -> io::Result<()> {
        self.set_i32(CID_POWER, i32::from(on))
    }

    /// Whether the bridge's supplies and clock are on.
    pub fn power(&self) -> io::Result<bool> {
        Ok(self.get_i32(CID_POWER)? != 0)
    }

    /// Sets how long the bridge waits for acknowledgements (10 ms to 10 s).
    pub fn set_ack_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_i32(
            CID_ACK_TIMEOUT_MS,
            timeout.as_millis().clamp(10, 10_000) as i32,
        )
    }

    /// The bridge's stream state.
    pub fn stream_state(&self) -> io::Result<StreamState> {
        StreamState::from_raw(self.get_i32(CID_STREAM_STATE)?)
    }

    /// The sequence number of the last stream request.
    pub fn last_sequence(&self) -> io::Result<u32> {
        Ok(self.get_i32(CID_STREAM_SEQUENCE)? as u32)
    }

    /// Subscribes this handle to stream requests. The bridge refuses to start the stream while no
    /// handle is subscribed.
    pub fn subscribe(&self) -> io::Result<()> {
        self.subdev
            .subscribe(STREAM_EVENT, 0, SubscribeFlags::empty())
            .map_err(io_error)
    }

    /// Unsubscribes (also happens when the handle is closed).
    pub fn unsubscribe(&self) -> io::Result<()> {
        self.subdev.unsubscribe(STREAM_EVENT, 0).map_err(io_error)
    }

    /// Dequeues a pending stream request without blocking; `None` if there is none.
    pub fn try_next_request(&self) -> io::Result<Option<StreamRequest>> {
        while let Some(ev) = self.subdev.dequeue_event().map_err(io_error)? {
            if let EventKind::Private { offset, data } = ev.kind
                && EventType::Private(offset) == STREAM_EVENT
            {
                return StreamRequest::decode(&data).map(Some);
            }
        }
        Ok(None)
    }

    /// Waits up to `timeout` (forever if `None`) for a stream request.
    pub fn next_request(&self, timeout: Option<Duration>) -> io::Result<Option<StreamRequest>> {
        if let Some(req) = self.try_next_request()? {
            return Ok(Some(req));
        }
        let ready =
            crate::ioctl::poll_fd(self.subdev.as_fd(), libc::POLLPRI, timeout).map_err(io_error)?;
        if !ready.priority {
            return Ok(None);
        }
        self.try_next_request()
    }

    /// Acknowledges `request`: `Ok(())` once the sensor has started or stopped, or `Err(errno)`
    /// if it could not (the receiver's STREAMON then fails with that error). Fails with `ESTALE`
    /// if the bridge already gave up waiting (timeout) or never asked.
    pub fn acknowledge(&self, request: &StreamRequest, result: Result<(), i32>) -> io::Result<()> {
        let value = ack_control_value(request.sequence, result);
        self.set_controls(&[(CID_STREAM_ACK, ControlValue::Integer64(value))])
    }

    /// Serves stream requests until `keep_going` returns false: calls `handler` for each request
    /// and acknowledges with its result. `poll_interval` bounds how often `keep_going` is checked.
    pub fn serve(
        &self,
        poll_interval: Duration,
        mut keep_going: impl FnMut() -> bool,
        mut handler: impl FnMut(&StreamRequest) -> Result<(), i32>,
    ) -> io::Result<()> {
        while keep_going() {
            if let Some(req) = self.next_request(Some(poll_interval))? {
                let result = handler(&req);
                match self.acknowledge(&req, result) {
                    Err(e) if e.raw_os_error() == Some(libc::ESTALE) => {}
                    other => other?,
                }
            }
        }
        Ok(())
    }
}

impl AsFd for SensorBridge {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.subdev.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StreamRequest {
        StreamRequest {
            action: StreamAction::Start,
            sequence: 42,
            timeout: Duration::from_millis(1000),
            link_freq: 400_000_000,
            pixel_rate: 160_000_000,
            code: 0x3007,
            width: 1280,
            height: 800,
            hblank: 176,
            vblank: 1022,
            data_lanes: 2,
            continuous_clock: true,
        }
    }

    #[test]
    fn stream_requests_round_trip() {
        let req = sample();
        assert_eq!(StreamRequest::decode(&req.encode()).unwrap(), req);
        let stop = StreamRequest {
            action: StreamAction::Stop,
            continuous_clock: false,
            ..req
        };
        assert_eq!(StreamRequest::decode(&stop.encode()).unwrap(), stop);
    }

    #[test]
    fn decodes_kernel_layout() {
        let d = sample().encode();
        // Field offsets of struct styx_bridge_stream_event.
        assert_eq!(u32_at(&d, 0), 1);
        assert_eq!(u32_at(&d, 4), ACTION_START);
        assert_eq!(u32_at(&d, 8), 42);
        assert_eq!(i64_at(&d, 16), 400_000_000);
        assert_eq!(u32_at(&d, 36), 1280);
        assert_eq!(u32_at(&d, 56), FLAG_CONTINUOUS_CLOCK);
    }

    #[test]
    fn rejects_unknown_versions_and_actions() {
        let mut d = sample().encode();
        d[0] = 9;
        assert!(StreamRequest::decode(&d).is_err());
        let mut d = sample().encode();
        d[4] = 7;
        assert!(StreamRequest::decode(&d).is_err());
    }

    #[test]
    fn encodes_acknowledgements() {
        assert_eq!(ack_control_value(5, Ok(())), 5);
        assert_eq!(
            ack_control_value(5, Err(libc::EIO)),
            (i64::from(libc::EIO) << 32) | 5
        );
        assert_eq!(
            ack_control_value(5, Err(-libc::EIO)),
            (i64::from(libc::EIO) << 32) | 5
        );
        assert_eq!(ack_control_value(5, Err(0)), (1 << 32) | 5);
        assert_eq!(ack_control_value(5, Err(100_000)), (4095 << 32) | 5);
    }

    #[test]
    fn parses_sysfs_locations() {
        let loc = parse_location(
            "/dev/v4l-subdev2".into(),
            "ov9782",
            "10",
            "0x60",
            "24000000",
        );
        assert_eq!(loc.i2c_bus, Some(10));
        assert_eq!(loc.i2c_address, Some(0x60));
        assert_eq!(loc.clock_frequency, 24_000_000);
        let none = parse_location("/dev/v4l-subdev0".into(), "sensor", "-1", "0x00", "0");
        assert_eq!((none.i2c_bus, none.i2c_address), (None, None));
    }

    /// Talks to real bridges, read-only (format, controls). Skips when none is bound.
    #[test]
    fn queries_present_bridges() {
        for loc in find_bridges().unwrap() {
            let bridge = SensorBridge::open(&loc.subdev).unwrap();
            assert!(!bridge.link_frequencies().is_empty());
            bridge.format().unwrap();
            bridge.stream_state().unwrap();
        }
    }

    #[test]
    fn opening_a_missing_node_fails() {
        assert!(SensorBridge::open("/dev/v4l-subdev-does-not-exist").is_err());
    }
}

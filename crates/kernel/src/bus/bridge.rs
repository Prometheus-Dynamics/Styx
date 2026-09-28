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

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::bridge_sys::*;
use super::ioctl::ioctl;

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
}

impl StreamState {
    fn from_raw(v: i32) -> io::Result<Self> {
        Ok(match v {
            0 => Self::Idle,
            1 => Self::Starting,
            2 => Self::Streaming,
            3 => Self::Stopping,
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
    file: File,
    path: PathBuf,
    link_frequencies: Vec<i64>,
}

impl SensorBridge {
    /// Opens a bridge subdev node (non-blocking; events are waited for with `poll`).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)?;
        let mut bridge = Self {
            file,
            path,
            link_frequencies: Vec::new(),
        };
        bridge.link_frequencies = bridge.query_int_menu(V4L2_CID_LINK_FREQ)?;
        Ok(bridge)
    }

    /// Node path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The link frequencies from the device tree, in Hz, indexed as [`Timing::link_freq_index`].
    pub fn link_frequencies(&self) -> &[i64] {
        &self.link_frequencies
    }

    fn query_int_menu(&self, id: u32) -> io::Result<Vec<i64>> {
        let mut q = V4l2Queryctrl {
            id,
            ..Default::default()
        };
        // SAFETY: VIDIOC_QUERYCTRL reads and fills one `v4l2_queryctrl`.
        unsafe { ioctl(self.file.as_fd(), VIDIOC_QUERYCTRL, &mut q) }?;
        let mut items = Vec::new();
        for index in q.minimum.max(0)..=q.maximum.max(0) {
            let mut m = V4l2Querymenu {
                id,
                index: index as u32,
                ..Default::default()
            };
            // SAFETY: VIDIOC_QUERYMENU reads and fills one `v4l2_querymenu`.
            unsafe { ioctl(self.file.as_fd(), VIDIOC_QUERYMENU, &mut m) }?;
            let v = m.value;
            let mut b = [0u8; 8];
            b.copy_from_slice(&v[..8]);
            items.push(i64::from_ne_bytes(b));
        }
        Ok(items)
    }

    /// Sets the active source pad format. The bridge clamps to its limits and replaces a code it
    /// does not accept; the result is what it chose. Fails with `EBUSY` while streaming.
    pub fn set_format(&self, format: PadFormat) -> io::Result<PadFormat> {
        let mut f = V4l2SubdevFormat {
            which: V4L2_SUBDEV_FORMAT_ACTIVE,
            ..Default::default()
        };
        f.format.code = format.code;
        f.format.width = format.width;
        f.format.height = format.height;
        f.format.field = 1; // V4L2_FIELD_NONE
        // SAFETY: VIDIOC_SUBDEV_S_FMT reads and fills one `v4l2_subdev_format`.
        unsafe { ioctl(self.file.as_fd(), VIDIOC_SUBDEV_S_FMT, &mut f) }?;
        Ok(PadFormat {
            code: f.format.code,
            width: f.format.width,
            height: f.format.height,
        })
    }

    /// The active source pad format.
    pub fn format(&self) -> io::Result<PadFormat> {
        let mut f = V4l2SubdevFormat {
            which: V4L2_SUBDEV_FORMAT_ACTIVE,
            ..Default::default()
        };
        // SAFETY: VIDIOC_SUBDEV_G_FMT reads and fills one `v4l2_subdev_format`.
        unsafe { ioctl(self.file.as_fd(), VIDIOC_SUBDEV_G_FMT, &mut f) }?;
        Ok(PadFormat {
            code: f.format.code,
            width: f.format.width,
            height: f.format.height,
        })
    }

    fn ext_ctrls(&self, request: u32, controls: &mut [V4l2ExtControl]) -> io::Result<()> {
        let mut c = V4l2ExtControls {
            which: V4L2_CTRL_WHICH_CUR_VAL,
            count: controls.len() as u32,
            error_idx: 0,
            request_fd: 0,
            reserved: [0],
            controls: controls.as_mut_ptr(),
        };
        // SAFETY: `c` describes `controls`, which stays alive and exclusively borrowed for the
        // call; the request is G/S_EXT_CTRLS, which reads and fills that array.
        unsafe { ioctl(self.file.as_fd(), request, &mut c) }?;
        Ok(())
    }

    fn set_i32(&self, id: u32, value: i32) -> io::Result<()> {
        self.ext_ctrls(
            VIDIOC_S_EXT_CTRLS,
            &mut [V4l2ExtControl::new_i32(id, value)],
        )
    }

    fn get_i32(&self, id: u32) -> io::Result<i32> {
        let mut c = [V4l2ExtControl::new_i32(id, 0)];
        self.ext_ctrls(VIDIOC_G_EXT_CTRLS, &mut c)?;
        Ok(c[0].value_i32())
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
        self.ext_ctrls(
            VIDIOC_S_EXT_CTRLS,
            &mut [
                V4l2ExtControl::new_i32(V4L2_CID_LINK_FREQ, timing.link_freq_index as i32),
                V4l2ExtControl::new_i64(V4L2_CID_PIXEL_RATE, timing.pixel_rate),
                V4l2ExtControl::new_i32(V4L2_CID_HBLANK, timing.hblank),
                V4l2ExtControl::new_i32(V4L2_CID_VBLANK, timing.vblank),
            ],
        )
    }

    /// Sets horizontal and vertical blanking only.
    pub fn set_blanking(&self, hblank: i32, vblank: i32) -> io::Result<()> {
        self.ext_ctrls(
            VIDIOC_S_EXT_CTRLS,
            &mut [
                V4l2ExtControl::new_i32(V4L2_CID_HBLANK, hblank),
                V4l2ExtControl::new_i32(V4L2_CID_VBLANK, vblank),
            ],
        )
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
        let mut sub = V4l2EventSubscription {
            type_: EVENT_STREAM,
            ..Default::default()
        };
        // SAFETY: VIDIOC_SUBSCRIBE_EVENT reads one `v4l2_event_subscription`.
        unsafe { ioctl(self.file.as_fd(), VIDIOC_SUBSCRIBE_EVENT, &mut sub) }?;
        Ok(())
    }

    /// Unsubscribes (also happens when the handle is closed).
    pub fn unsubscribe(&self) -> io::Result<()> {
        let mut sub = V4l2EventSubscription {
            type_: EVENT_STREAM,
            ..Default::default()
        };
        // SAFETY: VIDIOC_UNSUBSCRIBE_EVENT reads one `v4l2_event_subscription`.
        unsafe { ioctl(self.file.as_fd(), VIDIOC_UNSUBSCRIBE_EVENT, &mut sub) }?;
        Ok(())
    }

    /// Dequeues a pending stream request without blocking; `None` if there is none.
    pub fn try_next_request(&self) -> io::Result<Option<StreamRequest>> {
        loop {
            // SAFETY: all-zero bytes are a valid `v4l2_event`.
            let mut ev: V4l2Event = unsafe { std::mem::zeroed() };
            // SAFETY: VIDIOC_DQEVENT fills one `v4l2_event`, which `ev` is.
            match unsafe { ioctl(self.file.as_fd(), VIDIOC_DQEVENT, &mut ev) } {
                Ok(_) if ev.type_ == EVENT_STREAM => {
                    return StreamRequest::decode(&ev.u.data).map(Some);
                }
                Ok(_) => continue,
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Waits up to `timeout` (forever if `None`) for a stream request.
    pub fn next_request(&self, timeout: Option<Duration>) -> io::Result<Option<StreamRequest>> {
        if let Some(req) = self.try_next_request()? {
            return Ok(Some(req));
        }
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        // SAFETY: `pfd` is one live pollfd for the duration of the call.
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(err)
            };
        }
        if ret == 0 {
            return Ok(None);
        }
        self.try_next_request()
    }

    /// Acknowledges `request`: `Ok(())` once the sensor has started or stopped, or `Err(errno)`
    /// if it could not (the receiver's STREAMON then fails with that error). Fails with `ESTALE`
    /// if the bridge already gave up waiting (timeout) or never asked.
    pub fn acknowledge(&self, request: &StreamRequest, result: Result<(), i32>) -> io::Result<()> {
        let value = ack_control_value(request.sequence, result);
        self.ext_ctrls(
            VIDIOC_S_EXT_CTRLS,
            &mut [V4l2ExtControl::new_i64(CID_STREAM_ACK, value)],
        )
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
        self.file.as_fd()
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

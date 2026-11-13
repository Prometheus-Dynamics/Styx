//! A simulated camera: the native chain on Lemnos's simulated backend, through the real socket.
//!
//! [`SimCamera::start`] runs `lemnosd` in this process with the simulated drivers, finds the chain
//! by tag and link, starts it with contract calls in the order the Lemnos contracts set, and reads
//! the receiver's frames as `FrameLease`s. The simulated receiver makes frames but no pixels, so
//! each frame is rendered here, into the lease's own buffer, at the exposure and gain in force on
//! it. Controls are scheduled by frame identity: an update applies from the frame it names on.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lemnos_client::{Client, RemoteCaller};
use lemnos_contract_camera_isp::{CameraIsp, IspInput};
use lemnos_contract_camera_receiver::{CameraReceiver, Packing};
use lemnos_contract_camera_sensor::{CameraSensor, Controls, FrameValues};
use lemnos_core::{Bound, Status};
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_controls::{Control, ControlValue, FrameMetadata, Update, ValueError};
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_lemnos::{ChainError, Frames, find_chain, open_frames};

use crate::drivers::{self, SimIsp, SimReceiver, SimSensor};
use crate::scene::{self, Scene};

/// The simulated camera's name for the daemon and its client.
const NAME: &str = "styx-sim";

/// Digital gain is not simulated: every frame reports unity.
const DIGITAL_GAIN_MILLI: u32 = 1_000;

/// Errors from starting a simulated camera, setting a control and reading a frame.
#[derive(Debug)]
pub enum Error {
    /// The working directory or the daemon's board or service could not be made.
    Daemon(String),
    /// The daemon's client could not connect, list or open a contract.
    Client(lemnos_client::Error),
    /// The daemon's devices do not form one sensor, receiver and ISP chain.
    Chain(ChainError),
    /// A contract call on a simulated driver failed.
    Contract(Status),
    /// The frame stream could not be opened or read.
    Stream(styx_lemnos::Error),
    /// The scene does not fit the mode it was asked to render.
    Scene(String),
    /// The value is not one the control takes.
    Value(ValueError),
    /// The frame named is not after the last frame rendered (`rendered`), so it cannot apply.
    Late { frame: u64, rendered: u64 },
    /// The chain has no ISP; the simulated camera needs one to take its input.
    NoIsp,
    /// The frame's buffer cannot be written by the CPU.
    NotWritable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Daemon(e) => write!(f, "simulated daemon: {e}"),
            Error::Client(e) => write!(f, "simulated daemon client: {e}"),
            Error::Chain(e) => write!(f, "simulated chain: {e}"),
            Error::Contract(s) => write!(f, "simulated driver call failed: {s:?}"),
            Error::Stream(e) => write!(f, "simulated frame stream: {e}"),
            Error::Scene(e) => write!(f, "simulated scene: {e}"),
            Error::Value(e) => write!(f, "control value: {e:?}"),
            Error::Late { frame, rendered } => write!(
                f,
                "frame {frame} is not after the last frame rendered ({rendered})"
            ),
            Error::NoIsp => write!(f, "the simulated chain has no ISP"),
            Error::NotWritable => write!(f, "frame buffer is not writable by the CPU"),
        }
    }
}

impl std::error::Error for Error {}

/// One frame: its lease over the rendered buffer and the metadata in force on it.
pub struct SimFrame {
    lease: FrameLease<usize>,
    meta: FrameMetadata,
}

impl SimFrame {
    /// The frame's identity: its position in the stream, from 0.
    pub fn id(&self) -> u64 {
        self.lease.id()
    }

    /// `CLOCK_MONOTONIC` nanoseconds when the frame was made.
    pub fn timestamp_ns(&self) -> u64 {
        self.lease.timestamp_ns()
    }

    /// The controls in force on this frame, and what it measured.
    pub fn meta(&self) -> &FrameMetadata {
        &self.meta
    }

    /// The frame's lease, to read its planes or share it.
    pub fn lease(&self) -> &FrameLease<usize> {
        &self.lease
    }
}

impl fmt::Debug for SimFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimFrame")
            .field("id", &self.id())
            .field("timestamp_ns", &self.timestamp_ns())
            .field("meta", &self.meta)
            .finish()
    }
}

/// A simulated camera. Dropping it stops its drivers and the daemon.
pub struct SimCamera {
    sensor: Bound<dyn CameraSensor, RemoteCaller>,
    receiver: Bound<dyn CameraReceiver, RemoteCaller>,
    isp: Bound<dyn CameraIsp, RemoteCaller>,
    frames: Frames,
    /// Kept for the daemon's session: the drivers' contract handles are its clients.
    _client: Client,
    scene: Scene,
    width: u32,
    height: u32,
    /// The controls in force, by [`Control::index`]: the last update applied to each.
    in_force: [ControlValue; Control::COUNT],
    /// Updates not yet applied, in frame order.
    pending: VecDeque<Update>,
    /// The identity of the last frame rendered.
    last_id: Option<u64>,
    dir: PathBuf,
    /// Kept running for the drivers; last, so the daemon stops after the client and drivers.
    _service: Service,
}

impl fmt::Debug for SimCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimCamera")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("last_id", &self.last_id)
            .field("pending", &self.pending.len())
            .field("frames", &self.frames)
            .finish()
    }
}

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn unique_dir() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("styx-sim-{}-{n}", std::process::id()))
}

fn manual(value: ControlValue) -> Option<u32> {
    match value {
        ControlValue::Manual(v) => Some(v),
        _ => None,
    }
}

impl SimCamera {
    /// Starts the simulated camera in `mode` (0 is 320 by 40 at 120 frames a second, 1 is 640 by
    /// 40 at 60), showing `scene`.
    pub fn start(scene: Scene, mode: u16) -> Result<Self, Error> {
        let dir = unique_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::Daemon(e.to_string()))?;
        let result = Self::start_in(&dir, scene, mode);
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    fn start_in(dir: &Path, scene: Scene, mode: u16) -> Result<Self, Error> {
        let socket = dir.join("sock");
        let board = parse(&drivers::board(Some("csi0")))
            .map_err(|e| Error::Daemon(format!("board: {e:?}")))?;
        let config = Config {
            socket: socket.clone(),
            name: NAME.to_string(),
            board,
            drivers: vec![
                entry::<SimSensor>(),
                entry::<SimReceiver>(),
                entry::<SimIsp>(),
            ],
            backend: Box::new(SimBoard::new()),
            options: Options {
                call_deadline: Duration::from_millis(200),
                ..Options::default()
            },
        };
        let service = Service::start(config).map_err(|e| Error::Daemon(format!("{e:?}")))?;
        let client = Client::connect(&socket, NAME).map_err(Error::Client)?;
        let chain = find_chain(&client.list().map_err(Error::Client)?).map_err(Error::Chain)?;

        let mut sensor = client
            .open::<dyn CameraSensor>(&chain.sensor.name)
            .map_err(Error::Client)?;
        let mut receiver = client
            .open::<dyn CameraReceiver>(&chain.receiver.name)
            .map_err(Error::Client)?;
        let isp_device = chain.isp.as_ref().ok_or(Error::NoIsp)?;
        let mut isp = client
            .open::<dyn CameraIsp>(&isp_device.name)
            .map_err(Error::Client)?;

        // The Lemnos contract order: mode, select, configure, ISP input, stream, then start.
        let info = sensor.mode(mode).map_err(Error::Contract)?;
        sensor.select_mode(mode).map_err(Error::Contract)?;
        let layout = receiver
            .configure(info.link, Packing::Packed)
            .map_err(Error::Contract)?;
        isp.set_input(IspInput {
            width: layout.width,
            height: layout.height,
            bit_depth: layout.bit_depth,
            bayer: info.bayer,
        })
        .map_err(Error::Contract)?;
        if let Scene::Replay(frames) = &scene {
            let samples = layout.width as usize * layout.height as usize;
            if frames.is_empty() || frames.iter().any(|f| f.len() != samples) {
                return Err(Error::Scene(format!(
                    "replayed frames must be non-empty and {samples} samples each"
                )));
            }
        }
        let frames = open_frames(
            &client,
            chain.receiver.index,
            "frames",
            FourCc::RGGB10P,
            layout.width,
            layout.height,
        )
        .map_err(Error::Stream)?;
        receiver.start().map_err(Error::Contract)?;
        sensor.start().map_err(Error::Contract)?;

        let mut in_force = [ControlValue::Auto; Control::COUNT];
        for control in Control::ALL {
            in_force[control.index()] = control.spec().default;
        }
        Ok(Self {
            sensor,
            receiver,
            isp,
            frames,
            _client: client,
            scene,
            width: layout.width,
            height: layout.height,
            in_force,
            pending: VecDeque::new(),
            last_id: None,
            dir: dir.to_path_buf(),
            _service: service,
        })
    }

    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Frames the stream gave up because the consumer held every buffer.
    pub fn held_drops(&self) -> u64 {
        self.frames.held_drops()
    }

    /// Sets `update.control` to `update.value` from frame `update.frame` on. The sensor gets the
    /// value for that frame through its contract; the frame is rendered with it when it is read.
    ///
    /// Fails with [`Error::Late`] when `update.frame` has already been rendered, and with
    /// [`Error::Value`] when the control does not take the value. `Auto` on exposure or gain holds
    /// the reference value (the simulated sensor has no automatic exposure).
    pub fn set_control(&mut self, update: Update) -> Result<(), Error> {
        update
            .control
            .validate(update.value)
            .map_err(Error::Value)?;
        if let Some(rendered) = self.last_id
            && update.frame <= rendered
        {
            return Err(Error::Late {
                frame: update.frame,
                rendered,
            });
        }
        let controls = match update.control {
            Control::Exposure => Controls {
                exposure: Some(manual(update.value).unwrap_or(scene::REFERENCE_EXPOSURE_US)),
                ..Controls::default()
            },
            Control::Gain => Controls {
                analogue_gain: Some(manual(update.value).unwrap_or(scene::REFERENCE_GAIN_MILLI)),
                ..Controls::default()
            },
            _ => Controls::default(),
        };
        if controls != Controls::default() {
            self.sensor
                .post_values(FrameValues {
                    identity: update.frame,
                    controls,
                })
                .map_err(Error::Contract)?;
        }
        let at = self.pending.partition_point(|u| u.frame <= update.frame);
        self.pending.insert(at, update);
        Ok(())
    }

    /// Waits up to `timeout` for the next frame, renders it at the controls in force on it and
    /// returns it. `None` when no frame came in time.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<SimFrame>, Error> {
        let Some(mut lease) = self.frames.next(timeout).map_err(Error::Stream)? else {
            return Ok(None);
        };
        let id = lease.id();
        while let Some(update) = self.pending.front().copied()
            && update.frame <= id
        {
            self.pending.pop_front();
            self.in_force[update.control.index()] = update.value;
        }
        let exposure_us = manual(self.in_force[Control::Exposure.index()]);
        let gain_milli = manual(self.in_force[Control::Gain.index()]);
        let (exposure_us, gain_milli) = scene::resolve(exposure_us, gain_milli);
        self.render(&mut lease, id, exposure_us, gain_milli)?;
        self.last_id = Some(id);
        let meta = FrameMetadata {
            controls: self.in_force,
            sensor_timestamp_ns: lease.timestamp_ns(),
            exposure_us,
            analogue_gain_milli: gain_milli,
            digital_gain_milli: DIGITAL_GAIN_MILLI,
            lux: None,
            colour_temperature_k: None,
        };
        Ok(Some(SimFrame { lease, meta }))
    }

    /// Gives a frame back to the stream. Its buffer is reused once the lease is gone.
    pub fn release(&mut self, frame: SimFrame) {
        self.frames.release(frame.lease);
    }

    /// Writes frame `id` into the lease's buffer as `pRAA` samples at the given exposure and gain.
    fn render(
        &self,
        lease: &mut FrameLease<usize>,
        id: u64,
        exposure_us: u32,
        gain_milli: u32,
    ) -> Result<(), Error> {
        let (width, height) = (self.width, self.height);
        let row = scene::row_bytes(width);
        let mut planes = lease.write().ok_or(Error::NotWritable)?;
        let mut plane = planes.plane(0).ok_or(Error::NotWritable)?;
        for y in 0..height {
            let bytes = plane
                .row_bytes_mut(y as usize, row)
                .ok_or(Error::NotWritable)?;
            for (group, out) in bytes.as_chunks_mut::<5>().0.iter_mut().enumerate() {
                let mut samples = [0u16; 4];
                for (i, sample) in samples.iter_mut().enumerate() {
                    let x = group as u32 * 4 + i as u32;
                    let light = self.scene.light(id, x, y, width);
                    *sample = scene::sample(light, x, y, exposure_us, gain_milli);
                }
                scene::pack_quad(samples, out);
            }
        }
        Ok(())
    }
}

impl Drop for SimCamera {
    fn drop(&mut self) {
        let _ = self.sensor.stop();
        let _ = self.receiver.stop();
        let _ = self.isp.stop();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

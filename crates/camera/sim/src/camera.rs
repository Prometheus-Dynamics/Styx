//! A simulated camera: the native chain on Lemnos's simulated backend, through the real socket.
//!
//! [`SimCamera::start`] runs `lemnosd` in this process with the simulated drivers, finds the chain
//! by tag and link, starts it with contract calls in the order the Lemnos contracts set, and reads
//! the receiver's frames as `FrameLease`s. The simulated receiver makes frames but no pixels, so
//! each frame is rendered here, into the lease's own buffer, at the exposure and gain the sensor
//! latched on it.
//!
//! Time is the camera's frame clock (see `silicon`): [`SimCamera::next`] grants the next frame and
//! waits for it, so the values a write lands on are fixed by the frame it is written in and the
//! sensor's delay for its control. [`SimCamera::set_control`] names the frame a value must show
//! on, and the camera posts it through the sensor's contract during the frame before that by the
//! delay.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
use styx_lemnos::{ChainError, Frames, Geometry, find_chain, open_frames_writable_at};

use crate::drivers::{self, SimIsp, SimReceiver, SimSensor};
use crate::scene::{self, Scene};
use crate::silicon::{self, InForce, SensorTiming, Silicon};

/// The simulated camera's name for the daemon and its client.
const NAME: &str = "styx-sim";

/// Digital gain is not simulated: every frame reports unity.
const DIGITAL_GAIN_MILLI: u32 = 1_000;

/// How long [`SimCamera::next`] waits for the frame clock to grant a frame, at most.
const GRANT_WAIT: Duration = Duration::from_secs(10);

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
    /// The frame named cannot take the value: the write it needs is during a frame already past.
    /// `rendered` is the last frame latched.
    Late { frame: u64, rendered: u64 },
    /// The control is not one the simulated camera takes (exposure and gain).
    Unsupported(Control),
    /// A sensor delay of zero was asked for. A value written during a frame would have to show on
    /// that frame, which is already latched; delays are one frame or more.
    Timing,
    /// No settings were latched for a frame that was read.
    NoRecord(u64),
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
                "frame {frame} cannot take a value: the last frame latched is {rendered}"
            ),
            Error::Unsupported(c) => write!(f, "{} is not taken by the simulated camera", c.name()),
            Error::Timing => write!(
                f,
                "a sensor delay of zero cannot be honoured: delays are one frame or more"
            ),
            Error::NoRecord(id) => write!(f, "no settings were latched for frame {id}"),
            Error::NoIsp => write!(f, "the simulated chain has no ISP"),
            Error::NotWritable => write!(f, "frame buffer is not writable by the CPU"),
        }
    }
}

impl std::error::Error for Error {}

/// One frame: its lease over the rendered buffer and the metadata in force on it.
pub struct SimFrame {
    lease: FrameLease,
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
    pub fn lease(&self) -> &FrameLease {
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

/// A value waiting for its write frame: posted in frame `frame - delay`.
#[derive(Debug, Clone, Copy)]
struct Pending {
    update: Update,
    delay: i64,
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
    silicon: Arc<Silicon>,
    /// Frames granted to the stream so far: the next one to grant is `taken` when all were read.
    allowed: u64,
    /// Frames read so far.
    taken: u64,
    /// Updates not yet posted, in the order they were set.
    pending: VecDeque<Pending>,
    /// The name the drivers' config gives this camera's clock.
    token: String,
    dir: PathBuf,
    /// Kept running for the drivers; last, so the daemon stops after the client and drivers.
    _service: Service,
}

impl fmt::Debug for SimCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimCamera")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("taken", &self.taken)
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

impl SimCamera {
    /// Starts the simulated camera in `mode` (0 is 320 by 40, 1 is 640 by 40), showing `scene`,
    /// with the default sensor timing: exposure two frames on, gain one, frame length two.
    pub fn start(scene: Scene, mode: u16) -> Result<Self, Error> {
        Self::start_with(scene, mode, SensorTiming::default())
    }

    /// Starts the simulated camera with the sensor's delays in `timing`.
    pub fn start_with(scene: Scene, mode: u16, timing: SensorTiming) -> Result<Self, Error> {
        if !timing.is_honourable() {
            return Err(Error::Timing);
        }
        let dir = unique_dir();
        fs::create_dir_all(&dir).map_err(|e| Error::Daemon(e.to_string()))?;
        let token = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let silicon = Arc::new(Silicon::new(timing));
        silicon::register(&token, Arc::clone(&silicon));
        let result = Self::start_in(&dir, scene, mode, &token, silicon);
        silicon::unregister(&token);
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    fn start_in(
        dir: &Path,
        scene: Scene,
        mode: u16,
        token: &str,
        silicon: Arc<Silicon>,
    ) -> Result<Self, Error> {
        let socket = dir.join("sock");
        let board = parse(&drivers::board(token, Some("csi0")))
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
        let isp_device = chain.isps.first().ok_or(Error::NoIsp)?;
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
        let geometry = Geometry {
            code: FourCc::RGGB10P,
            width: layout.width,
            height: layout.height,
            stride: layout.stride,
        };
        let frames = open_frames_writable_at(&client, chain.receiver.index, "frames", geometry)
            .map_err(Error::Stream)?;
        receiver.start().map_err(Error::Contract)?;
        sensor.start().map_err(Error::Contract)?;

        Ok(Self {
            sensor,
            receiver,
            isp,
            frames,
            _client: client,
            scene,
            width: layout.width,
            height: layout.height,
            silicon,
            allowed: 0,
            taken: 0,
            pending: VecDeque::new(),
            token: token.to_string(),
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

    /// The sensor's delays, as the mode reports them.
    pub fn timing(&self) -> SensorTiming {
        self.silicon.timing()
    }

    /// The camera's sensor, as its `camera.sensor/1` contract. A caller may post values to it
    /// directly: they are written during the last frame read, as [`SimCamera::next`] describes.
    pub fn sensor(&mut self) -> &mut dyn CameraSensor {
        &mut self.sensor
    }

    /// Frames the stream gave up because the consumer held every buffer.
    pub fn held_drops(&self) -> u64 {
        self.frames.held_drops()
    }

    /// Sets `update.control` to `update.value` from frame `update.frame` on. The camera posts it
    /// through the sensor's contract during frame `update.frame - delay`, so it shows on that frame
    /// (the delay is the sensor's, per control).
    ///
    /// Fails with [`Error::Late`] when the write frame has passed, and with [`Error::Value`] when
    /// the control does not take the value. `Auto` on exposure or gain holds the reference value.
    pub fn set_control(&mut self, update: Update) -> Result<(), Error> {
        let lane = silicon::lane_of(update.control).ok_or(Error::Unsupported(update.control))?;
        update
            .control
            .validate(update.value)
            .map_err(Error::Value)?;
        let delay = self.silicon.timing().delay(lane) as i64;
        let cur = self.silicon.granted() as i64 - 1;
        if update.frame as i64 - delay < cur {
            return Err(Error::Late {
                frame: update.frame,
                rendered: cur.max(0) as u64,
            });
        }
        let at = self
            .pending
            .partition_point(|p| p.update.frame <= update.frame);
        self.pending.insert(at, Pending { update, delay });
        Ok(())
    }

    /// Posts each pending value whose write frame is `cur`, the last frame latched.
    fn post_due(&mut self, cur: i64) -> Result<(), Error> {
        let mut i = 0;
        while i < self.pending.len() {
            let p = self.pending[i];
            if p.update.frame as i64 - p.delay != cur {
                i += 1;
                continue;
            }
            self.pending.remove(i);
            let code = match p.update.value {
                ControlValue::Manual(us_or_milli) => match p.update.control {
                    Control::Exposure => silicon::exposure_code(us_or_milli),
                    _ => silicon::gain_code(us_or_milli),
                },
                _ => match p.update.control {
                    Control::Exposure => silicon::exposure_code(scene::REFERENCE_EXPOSURE_US),
                    _ => silicon::gain_code(scene::REFERENCE_GAIN_MILLI),
                },
            };
            let controls = match p.update.control {
                Control::Exposure => Controls {
                    exposure: Some(code),
                    ..Controls::default()
                },
                _ => Controls {
                    analogue_gain: Some(code),
                    ..Controls::default()
                },
            };
            self.sensor
                .post_values(FrameValues {
                    identity: p.update.frame,
                    controls,
                })
                .map_err(Error::Contract)?;
        }
        Ok(())
    }

    /// Grants the next frame, waits up to `timeout` for it, renders it at the settings the sensor
    /// latched on it and returns it. `None` when no frame came in time.
    ///
    /// Values posted through the contract (or by [`SimCamera::set_control`]) since the last frame
    /// was read are written during that frame; the frame latched next shows the ones due on it.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<SimFrame>, Error> {
        let granted = self
            .silicon
            .await_granted(self.allowed, timeout.min(GRANT_WAIT));
        if granted < self.allowed {
            return Ok(None);
        }
        self.post_due(granted as i64 - 1)?;
        self.allowed = self.taken + 1;
        self.silicon.allow(self.allowed);
        let Some(mut lease) = self.frames.next(timeout).map_err(Error::Stream)? else {
            return Ok(None);
        };
        let id = lease.id();
        let in_force = self.silicon.in_force(id).ok_or(Error::NoRecord(id))?;
        let (exposure_code, gain_code) = codes_in_force(in_force);
        self.render(&mut lease, id, exposure_code, gain_code)?;
        self.taken = id + 1;
        let exposure_us = silicon::exposure_us(exposure_code);
        let gain_milli = silicon::gain_milli(gain_code);
        let mut controls = [ControlValue::Auto; Control::COUNT];
        for control in Control::ALL {
            controls[control.index()] = control.spec().default;
        }
        controls[Control::Exposure.index()] = in_force
            .exposure
            .map_or(ControlValue::Auto, |_| ControlValue::Manual(exposure_us));
        controls[Control::Gain.index()] = in_force
            .gain
            .map_or(ControlValue::Auto, |_| ControlValue::Manual(gain_milli));
        let meta = FrameMetadata {
            controls,
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

    /// Writes frame `id` into the lease's buffer as `pRAA` samples at the scale the codes in force
    /// give against the reference exposure and gain.
    fn render(
        &self,
        lease: &mut FrameLease,
        id: u64,
        exposure_code: u32,
        gain_code: u32,
    ) -> Result<(), Error> {
        let (width, height) = (self.width, self.height);
        let row = scene::row_bytes(width);
        let scale = scale_of(exposure_code, gain_code);
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
                    *sample = scene::sample(light, x, y, scale);
                }
                scene::pack_quad(samples, out);
            }
        }
        Ok(())
    }
}

/// The exposure and gain codes in force on a frame, with the reference for an automatic control.
fn codes_in_force(in_force: InForce) -> (u32, u32) {
    (
        in_force
            .exposure
            .unwrap_or_else(|| silicon::exposure_code(scene::REFERENCE_EXPOSURE_US)),
        in_force
            .gain
            .unwrap_or_else(|| silicon::gain_code(scene::REFERENCE_GAIN_MILLI)),
    )
}

/// The brightness scale of a frame against the reference exposure and gain.
fn scale_of(exposure_code: u32, gain_code: u32) -> f32 {
    let ref_exposure = silicon::exposure_code(scene::REFERENCE_EXPOSURE_US);
    let ref_gain = silicon::gain_code(scene::REFERENCE_GAIN_MILLI);
    exposure_code as f32 / ref_exposure as f32 * (gain_code as f32 / ref_gain as f32)
}

impl Drop for SimCamera {
    fn drop(&mut self) {
        let _ = self.sensor.stop();
        let _ = self.receiver.stop();
        let _ = self.isp.stop();
        silicon::unregister(&self.token);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

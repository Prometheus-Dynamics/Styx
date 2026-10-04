//! A camera on a platform: its receiver and its sensor side, started and stopped in the order
//! every path ends in a clean state (buffers back with the receiver or released, the sensor
//! in standby).
//!
//! ```text
//! start   receiver.configure (buffers) → queue every buffer → receiver.start (the sensor is
//!         started by the receiver: before it, after it, or through its own machinery,
//!         StartOrder) → FrameStream
//!         on failure: receiver.release, sensor standby
//! stop    pool retired (held frames keep their buffers) → receiver.stop → receiver.release
//!         → sensor standby
//! ```

use core::fmt;

use styx_hal::{
    Configured, ErrorKind, HalError, MaybeSendSync, Receiver, ReceiverConfig, SensorStart,
};

use crate::error::Error;
use crate::health::Health;
use crate::side::SensorSide;
use crate::stream::{FrameStream, Pool};
use crate::sync::Ref;

/// What a camera runs on: one receiver type and one sensor side per binary (on Linux the
/// sensor side is `dyn SensorSide`, so host tests run it over fake devices; on a
/// microcontroller it is the concrete `Lock<SensorState<Bus, Pins>>`).
pub trait Platform: Sized + 'static {
    /// The receiver.
    type Receiver: Receiver + 'static;
    /// The sensor side ([`SensorSide`]).
    type Sensor: SensorSide + ?Sized;

    /// Called before each start, with the stream's health: a receiver whose own machinery
    /// serves the sensor (frame starts, start and stop requests: [`StartOrder::ReceiverDriven`]
    /// on Linux) takes the sensor side and the health here. Nothing by default.
    ///
    /// [`StartOrder::ReceiverDriven`]: styx_hal::StartOrder::ReceiverDriven
    fn attach(receiver: &Self::Receiver, sensor: &Ref<Self::Sensor>, health: &Ref<Health>) {
        let _ = (receiver, sensor, health);
    }
}

/// An error of a running camera: the receiver's own error, or the runtime's.
#[derive(Debug)]
pub enum RunError<E> {
    /// The receiver failed.
    Receiver(E),
    /// The runtime: a fault that ended the stream, a disconnect, the camera's state.
    Runtime(Error),
}

impl<E: fmt::Display> fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Receiver(e) => e.fmt(f),
            RunError::Runtime(e) => e.fmt(f),
        }
    }
}

impl<E: HalError> RunError<E> {
    /// What kind of failure.
    pub fn kind(&self) -> ErrorKind {
        match self {
            RunError::Receiver(e) => e.kind(),
            RunError::Runtime(e) => e.kind(),
        }
    }
}

impl<E> From<Error> for RunError<E> {
    fn from(e: Error) -> Self {
        RunError::Runtime(e)
    }
}

/// How a camera streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraOptions {
    /// Frame starts come from the receiver's events (else they are inferred from the frames).
    pub frame_sync: bool,
    /// Frames in a row the receiver may flag as corrupted before the stream ends with an error
    /// (0: never).
    pub max_error_frames: u32,
}

impl Default for CameraOptions {
    fn default() -> Self {
        Self {
            frame_sync: true,
            max_error_frames: 30,
        }
    }
}

/// The sensor side as a receiver starts and stops it ([`SensorStart`]).
pub struct SensorHandle<S: ?Sized>(pub Ref<S>);

impl<S: ?Sized> Clone for SensorHandle<S> {
    fn clone(&self) -> Self {
        Self(Ref::clone(&self.0))
    }
}

impl<S: SensorSide + ?Sized> SensorStart for SensorHandle<S>
where
    Ref<S>: MaybeSendSync,
{
    fn start(&self) -> Result<(), ErrorKind> {
        self.0.serve_start(None).map_err(|e| e.kind)
    }

    fn stop(&self) -> Result<(), ErrorKind> {
        self.0.serve_stop().map_err(|e| e.kind)
    }
}

struct Running<P: Platform> {
    pool: Ref<Pool<P::Receiver>>,
    health: Ref<Health>,
}

/// A camera: a receiver and a sensor side, configured by the platform (the sensor brought up,
/// the receiver path set up), started and stopped here.
pub struct Camera<P: Platform> {
    receiver: Ref<P::Receiver>,
    sensor: Ref<P::Sensor>,
    options: CameraOptions,
    running: Option<Running<P>>,
}

impl<P: Platform> fmt::Debug for Camera<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Camera")
            .field("options", &self.options)
            .field("streaming", &self.is_streaming())
            .finish_non_exhaustive()
    }
}

/// The receiver error of a camera.
pub type CameraError<P> = RunError<<<P as Platform>::Receiver as Receiver>::Error>;

impl<P: Platform> Camera<P>
where
    SensorHandle<P::Sensor>: SensorStart,
{
    /// A camera over `receiver` and `sensor`.
    pub fn new(receiver: Ref<P::Receiver>, sensor: Ref<P::Sensor>, options: CameraOptions) -> Self {
        Self {
            receiver,
            sensor,
            options,
            running: None,
        }
    }

    /// The receiver.
    pub fn receiver(&self) -> &Ref<P::Receiver> {
        &self.receiver
    }

    /// The sensor side.
    pub fn sensor(&self) -> &Ref<P::Sensor> {
        &self.sensor
    }

    /// The options.
    pub fn options(&self) -> &CameraOptions {
        &self.options
    }

    /// Changes the options (from the next start).
    pub fn set_options(&mut self, options: CameraOptions) {
        self.options = options;
    }

    /// Whether frames are streaming.
    pub fn is_streaming(&self) -> bool {
        self.running.is_some()
    }

    /// The running stream's health.
    pub fn health(&self) -> Option<&Ref<Health>> {
        self.running.as_ref().map(|r| &r.health)
    }

    /// Starts streaming with `config`: returns once the receiver (and through it the sensor)
    /// started. On failure everything started is undone: buffers released, the sensor in
    /// standby.
    pub fn start(
        &mut self,
        config: &ReceiverConfig,
    ) -> Result<(FrameStream<P>, Configured), CameraError<P>> {
        if self.running.is_some() {
            return Err(Error::Busy("already streaming".into()).into());
        }
        let health = Ref::new(Health::default());
        P::attach(&self.receiver, &self.sensor, &health);
        let configured = self
            .receiver
            .configure(config)
            .map_err(RunError::Receiver)?;
        let pool = match Pool::new(Ref::clone(&self.receiver), configured.buffers) {
            Some(p) => Ref::new(p),
            None => {
                let _ = self.receiver.release();
                return Err(Error::State("the receiver has fewer buffers than it said").into());
            }
        };
        // Dropping the pool leaves the buffers with the receiver; release them on failure.
        if let Err(e) = pool.queue_all() {
            let _ = self.receiver.release();
            return Err(RunError::Receiver(e));
        }
        if let Err(e) = self.receiver.start(SensorHandle(Ref::clone(&self.sensor))) {
            pool.retire();
            let _ = self.receiver.release();
            self.sensor.standby();
            return Err(RunError::Receiver(e));
        }
        self.running = Some(Running {
            pool: Ref::clone(&pool),
            health: Ref::clone(&health),
        });
        let frames = FrameStream::new(
            Ref::clone(&self.receiver),
            pool,
            Ref::clone(&self.sensor),
            health,
            self.options.frame_sync,
            self.options.max_error_frames,
        );
        Ok((frames, configured))
    }

    /// Stops streaming. Frame streams end; frames still held keep their memory until dropped
    /// but never go back to the receiver, whose buffers are released here.
    pub fn stop(&mut self) -> Result<(), CameraError<P>> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        running.pool.retire();
        let stopped = self.receiver.stop(SensorHandle(Ref::clone(&self.sensor)));
        let released = self.receiver.release();
        self.sensor.standby();
        match stopped {
            // The device is gone: there is nothing left to stop.
            Err(e) if e.kind() == ErrorKind::Disconnected => {
                Err(RunError::Runtime(Error::Disconnected))
            }
            Err(e) => Err(RunError::Receiver(e)),
            Ok(()) => released.map_err(RunError::Receiver),
        }
    }

    /// Stops, puts the sensor in standby and powers it down. Every step runs; the first error
    /// is returned.
    pub fn shut_down(&mut self) -> Result<(), CameraError<P>> {
        let stopped = self.stop();
        let down = self.sensor.shut_down();
        stopped.and(down.map_err(RunError::Runtime))
    }
}

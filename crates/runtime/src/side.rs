//! The sensor side as the rest of a running camera calls it: whatever serves frame starts (a
//! receiver's own event machinery, a sensor task), the frame stream (the values that produced
//! a frame, frame starts inferred from dequeues) and the camera (standby, shut-down).

use alloc::string::{String, ToString};
use core::time::Duration;

use styx_hal::{ErrorKind, HalError, MaybeSendSync, Receiver, SyncEvent};
use styx_sensor::{DriverBus, SensorPins};

use crate::health::{Fault, Health};
use crate::sensor::{FrameControls, SensorState, ServeError, StartFormat};
use crate::sync::{Lock, MaybeSend, lock};

/// The sensor side of a running camera: the shared [`SensorState`] (`Lock<SensorState<B, P>>`
/// implements it), as receivers, frame streams and cameras call it. Every method locks for
/// the duration of the call.
pub trait SensorSide: MaybeSendSync + 'static {
    /// Serves the receiver's start (carrying `format` when the receiver says what it was
    /// configured for): writes stream-on, starts the control schedule.
    fn serve_start(&self, format: Option<&StartFormat>) -> Result<(), ServeError>;
    /// Serves the receiver's stop: writes stream-off.
    fn serve_stop(&self) -> Result<(), ServeError>;
    /// The receiver starts the sensor itself (a kernel driver's): frame 0's values are set,
    /// the control schedule starts.
    fn start_streaming(&self) -> Result<(), String>;
    /// A frame started (at `at` on the platform clock, when known): writes what is due.
    fn frame_start(&self, seq: u64, at: Option<Duration>) -> Result<(), String>;
    /// The values that produced frame `seq`.
    fn applied(&self, seq: u64) -> Option<FrameControls>;
    /// Requested values wait for a coming frame start.
    fn writes_pending(&self) -> bool;
    /// Embedded data of frame `seq`.
    fn report_embedded(&self, seq: u64, data: &[u8]);
    /// Puts the sensor back in standby if it streams.
    fn standby(&self);
    /// Standby and power down, whatever the state.
    fn shut_down(&self) -> crate::Result<()>;
}

impl<B, P> SensorSide for Lock<SensorState<B, P>>
where
    B: DriverBus + MaybeSend + 'static,
    P: SensorPins + MaybeSend + 'static,
    Lock<SensorState<B, P>>: MaybeSendSync,
{
    fn serve_start(&self, format: Option<&StartFormat>) -> Result<(), ServeError> {
        lock(self).serve_start(format)
    }

    fn serve_stop(&self) -> Result<(), ServeError> {
        lock(self).serve_stop()
    }

    fn start_streaming(&self) -> Result<(), String> {
        lock(self).start_streaming().map_err(|e| e.to_string())
    }

    fn frame_start(&self, seq: u64, at: Option<Duration>) -> Result<(), String> {
        lock(self)
            .frame_start_at(seq, at)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn applied(&self, seq: u64) -> Option<FrameControls> {
        lock(self).applied(seq)
    }

    fn writes_pending(&self) -> bool {
        lock(self).writes_pending()
    }

    fn report_embedded(&self, seq: u64, data: &[u8]) {
        let _ = lock(self).report_embedded(seq, data);
    }

    fn standby(&self) {
        lock(self).standby();
    }

    fn shut_down(&self) -> crate::Result<()> {
        lock(self).shut_down()
    }
}

/// Where frame starts come from: a receiver (every [`Receiver`] is one), or the part of one
/// that reports them (the Linux event thread's frame-start node).
pub trait SyncSource {
    /// The error.
    type Error: HalError;
    /// The next frame start or embedded data event, without waiting.
    fn try_sync(&self) -> Result<Option<SyncEvent>, Self::Error>;
    /// Embedded data in `slot`.
    fn embedded(&self, slot: u32) -> Option<&[u8]> {
        let _ = slot;
        None
    }
}

impl<R: Receiver + ?Sized> SyncSource for R {
    type Error = R::Error;
    fn try_sync(&self) -> Result<Option<SyncEvent>, R::Error> {
        Receiver::try_sync(self)
    }
    fn embedded(&self, slot: u32) -> Option<&[u8]> {
        Receiver::embedded(self, slot)
    }
}

/// The sensor service: feeds every pending frame-start event of `receiver` to the control
/// schedule (writing what is due) and reports embedded data, without waiting. `Err` when the
/// receiver went away; other receiver errors are left to the frame stream.
///
/// Whatever waits for the receiver's frame starts calls it when they may be pending: the Linux
/// event thread after its `poll(2)`, a pipeline thread that serves them itself ("driven"), a
/// sensor task after [`Receiver::poll_sync`], a superloop each turn.
pub fn serve_sync<R, S>(receiver: &R, sensor: &S, health: &Health) -> Result<(), Fault>
where
    R: SyncSource + ?Sized,
    S: SensorSide + ?Sized,
{
    loop {
        match receiver.try_sync() {
            Ok(Some(event)) => sync_event(receiver, sensor, health, event),
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Disconnected => {
                return Err(Fault::disconnected(alloc::format!(
                    "frame-start event: {e}"
                )));
            }
            // Other errors (a queue that is not streaming) are left to the frame stream.
            Err(_) => return Ok(()),
        }
    }
}

/// Applies one sync event (see [`serve_sync`]).
pub fn sync_event<R, S>(receiver: &R, sensor: &S, health: &Health, event: SyncEvent)
where
    R: SyncSource + ?Sized,
    S: SensorSide + ?Sized,
{
    match event {
        SyncEvent::FrameStart { sequence, at } => {
            health.frame_syncs.incr();
            let at = Duration::from_nanos(at.as_nanos());
            health.control_write(sensor.frame_start(sequence, Some(at)));
        }
        SyncEvent::Embedded {
            sequence,
            slot,
            bytes,
        } => {
            if let Some(data) = receiver.embedded(slot) {
                sensor.report_embedded(sequence, &data[..bytes.min(data.len())]);
            }
        }
        SyncEvent::Glitch(_) => health.glitches.incr(),
    }
}

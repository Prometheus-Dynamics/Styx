//! The simulated sensor's frame clock and settings, shared by the sensor, the receiver and the
//! camera in this process.
//!
//! The clock is lock-step. The camera's `next` grants the next frame; the receiver latches that
//! frame (its settings are fixed then) and publishes it. Between two frames the caller may post
//! values, and they are written during the last frame latched. A value written during frame `w`
//! shows from frame `w + delay` on, where `delay` is the sensor's delay for that control, as
//! `camera.sensor/1` says: frames from a write to the first frame it shows on. A delay of zero
//! would show a value on a frame that is already latched, so the simulation refuses one.
//!
//! The clock does not run on time: a frame is made when it is granted. A free-running clock would
//! make the frame a write lands on depend on when the host ran the write, and no test could say
//! which frame that was.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use styx_controls::Control;

/// Lanes: exposure, analogue gain and frame length, in that order.
pub(crate) const EXPOSURE: usize = 0;
pub(crate) const GAIN: usize = 1;
pub(crate) const FRAME_LENGTH: usize = 2;
pub(crate) const LANES: usize = 3;

/// Picoseconds per line of every mode.
pub(crate) const LINE_TIME_PS: u64 = 9_100_000;
/// Fractional bits of the exposure code: codes count sixteenths of a line.
pub(crate) const EXPOSURE_FRACTION_BITS: u8 = 4;
/// The gain code for a gain of one.
pub(crate) const GAIN_UNITY: u32 = 16;

/// Frames the history keeps, per lane, past the last landing the consumer can still need.
const HISTORY: usize = 256;
/// Latched frames kept for the consumer to read back, by identity.
const FRAMES: usize = 16;

/// Frames from a write to the first frame it shows on, per control (the mode's delays).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorTiming {
    /// Frames from a write of exposure to the first frame it shows on.
    pub exposure_delay: u8,
    /// Frames from a write of analogue gain to the first frame it shows on.
    pub gain_delay: u8,
    /// Frames from a write of frame length to the first frame it shows on.
    pub frame_length_delay: u8,
}

impl Default for SensorTiming {
    /// The delays of a sensor that shows exposure two frames on and gain one frame on, as the
    /// usual sensors do.
    fn default() -> Self {
        Self {
            exposure_delay: 2,
            gain_delay: 1,
            frame_length_delay: 2,
        }
    }
}

impl SensorTiming {
    pub(crate) fn delay(&self, lane: usize) -> u64 {
        u64::from(match lane {
            EXPOSURE => self.exposure_delay,
            GAIN => self.gain_delay,
            _ => self.frame_length_delay,
        })
    }

    /// Whether every delay is one frame or more.
    pub fn is_honourable(&self) -> bool {
        self.exposure_delay >= 1 && self.gain_delay >= 1 && self.frame_length_delay >= 1
    }
}

/// The codes in force on one latched frame. `None` is automatic: the reference value applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct InForce {
    pub(crate) exposure: Option<u32>,
    pub(crate) gain: Option<u32>,
    pub(crate) frame_length: Option<u32>,
}

struct State {
    /// Frames latched so far; the next frame to latch is `granted`.
    granted: u64,
    /// Frames the camera has granted, through its `next` calls.
    allowed: u64,
    /// Per lane: `(first frame shown, code)` in landing order. Starts at frame 0 with automatic.
    lanes: [VecDeque<(u64, Option<u32>)>; LANES],
    /// Settings of recently latched frames, by identity.
    frames: VecDeque<(u64, InForce)>,
}

/// One simulated camera's clock, timing and settings.
pub(crate) struct Silicon {
    timing: SensorTiming,
    state: Mutex<State>,
    changed: Condvar,
}

impl std::fmt::Debug for Silicon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Silicon")
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

fn start_lane() -> VecDeque<(u64, Option<u32>)> {
    let mut lane = VecDeque::with_capacity(HISTORY + 1);
    lane.push_back((0, None));
    lane
}

impl Silicon {
    pub(crate) fn new(timing: SensorTiming) -> Self {
        Self {
            timing,
            state: Mutex::new(State {
                granted: 0,
                allowed: 0,
                lanes: [start_lane(), start_lane(), start_lane()],
                frames: VecDeque::with_capacity(FRAMES + 1),
            }),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn timing(&self) -> SensorTiming {
        self.timing
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Frames latched so far.
    pub(crate) fn granted(&self) -> u64 {
        self.lock().granted
    }

    /// Lets the receiver latch frames up to `count` (frames `0..count`).
    pub(crate) fn allow(&self, count: u64) {
        let mut state = self.lock();
        state.allowed = state.allowed.max(count);
        self.changed.notify_all();
    }

    /// Waits until `count` frames are latched, up to `timeout`. Returns the frames latched.
    pub(crate) fn await_granted(&self, count: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while state.granted < count {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
        }
        state.granted
    }

    /// Latches the next frame if the camera has granted it, and returns its identity.
    pub(crate) fn latch(&self) -> Option<u64> {
        let mut state = self.lock();
        if state.granted >= state.allowed {
            return None;
        }
        let id = state.granted;
        let mut in_force = InForce::default();
        let mut codes = [None; LANES];
        for (lane, code) in state.lanes.iter().zip(codes.iter_mut()) {
            *code = lane
                .iter()
                .rev()
                .find(|&&(first, _)| first <= id)
                .or(lane.front())
                .and_then(|&(_, code)| code);
        }
        in_force.exposure = codes[EXPOSURE];
        in_force.gain = codes[GAIN];
        in_force.frame_length = codes[FRAME_LENGTH];
        if state.frames.len() == FRAMES {
            state.frames.pop_front();
        }
        state.frames.push_back((id, in_force));
        state.granted += 1;
        self.changed.notify_all();
        Some(id)
    }

    /// The codes in force on the newest latched frame, if one is latched.
    pub(crate) fn latest_in_force(&self) -> Option<InForce> {
        self.lock().frames.back().map(|&(_, in_force)| in_force)
    }

    /// The code last written to `lane`, whether or not it has landed yet. `None` when nothing was
    /// written (automatic).
    pub(crate) fn last_written(&self, lane: usize) -> Option<u32> {
        self.lock().lanes[lane].back().and_then(|&(_, code)| code)
    }

    /// The codes in force on frame `id`, once it is latched.
    pub(crate) fn in_force(&self, id: u64) -> Option<InForce> {
        self.lock()
            .frames
            .iter()
            .find(|&&(frame, _)| frame == id)
            .map(|&(_, in_force)| in_force)
    }

    /// Writes `code` to `lane` during the last latched frame: it shows from that frame plus the
    /// lane's delay. Before any frame is latched, the write is during frame -1.
    pub(crate) fn post(&self, lane: usize, code: Option<u32>) {
        let mut state = self.lock();
        let landing = (state.granted + self.timing.delay(lane)).saturating_sub(1);
        push(&mut state.lanes[lane], landing, code);
    }

    /// Writes `code` to `lane` to show on the next frame to be latched, whatever the delay.
    pub(crate) fn set_now(&self, lane: usize, code: Option<u32>) {
        let mut state = self.lock();
        let landing = state.granted;
        push(&mut state.lanes[lane], landing, code);
    }
}

fn push(lane: &mut VecDeque<(u64, Option<u32>)>, landing: u64, code: Option<u32>) {
    // A write on a frame the lane already shows from replaces that entry: the later write wins.
    if let Some(back) = lane.back_mut()
        && back.0 == landing
    {
        back.1 = code;
        return;
    }
    lane.push_back((landing, code));
    if lane.len() > HISTORY {
        lane.pop_front();
    }
}

/// Simulated cameras by the name their driver config gives, so the sensor, receiver and camera of
/// one camera find the same clock.
static REGISTRY: Mutex<Vec<(String, Arc<Silicon>)>> = Mutex::new(Vec::new());

pub(crate) fn register(name: &str, silicon: Arc<Silicon>) {
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((name.to_string(), silicon));
}

pub(crate) fn unregister(name: &str) {
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(n, _)| n != name);
}

pub(crate) fn lookup(name: &str) -> Option<Arc<Silicon>> {
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, s)| Arc::clone(s))
}

/// Exposure code for `us` microseconds at the mode's line time, rounded.
pub(crate) fn exposure_code(us: u32) -> u32 {
    let scale = 1_000_000u64 << EXPOSURE_FRACTION_BITS;
    ((u64::from(us) * scale + LINE_TIME_PS / 2) / LINE_TIME_PS) as u32
}

/// Microseconds for an exposure code, rounded.
pub(crate) fn exposure_us(code: u32) -> u32 {
    let den = 1_000_000u64 << EXPOSURE_FRACTION_BITS;
    ((u64::from(code) * LINE_TIME_PS + den / 2) / den) as u32
}

/// Gain code for `milli` thousandths of gain, rounded.
pub(crate) fn gain_code(milli: u32) -> u32 {
    (u64::from(milli) * u64::from(GAIN_UNITY) + 500) as u32 / 1000
}

/// Thousandths of gain for a gain code, rounded.
pub(crate) fn gain_milli(code: u32) -> u32 {
    (u64::from(code) * 1000 + u64::from(GAIN_UNITY) / 2) as u32 / GAIN_UNITY
}

/// The lane a control is written on, for the controls the simulation takes (exposure and gain).
pub(crate) fn lane_of(control: Control) -> Option<usize> {
    match control {
        Control::Exposure => Some(EXPOSURE),
        Control::Gain => Some(GAIN),
        _ => None,
    }
}

//! The mock platform: what a real port replaces. A replay receiver (frames from a raw
//! recording, re-exposed with the values the sensor's control schedule put on each frame, into
//! buffers the receiver owns), the OV9782 as a register model on a "bus", pins without roles,
//! and the platform clock. Everything here is what a microcontroller port writes for its
//! peripherals (a DCMI/CSI receiver, an I²C bus, GPIOs, a timer); the firmware above it
//! ([`crate::firmware`]) stays as it is.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use portable_atomic::{AtomicU64, Ordering};
use spin::Mutex;
use styx_hal::embedded_hal::delay::DelayNs;
use styx_hal::{
    Access, BufferSource, Bus, Configured, ErrorKind, FrameBuffer, FrameDone, Instant, Receiver,
    ReceiverCaps, ReceiverConfig, SensorStart, StartOrder, SyncEvent, TimestampPoint,
};
use styx_pipeline::reexpose::{Recorded, exposure_ratio, re_expose};
use styx_pipeline::{SensorValues, process::sensor_values};
use styx_runtime::sync::{Lock, Ref, Shared, lock};
use styx_runtime::{Platform, SensorSide, SensorState};
use styx_sensor::lemnos_hal::register::RegisterResult;
use styx_sensor::{BusResult, DriverBus, RegisterBus, SensorDescription, SensorDriver, SensorPins};

/// The platform clock in nanoseconds (a hardware timer on a microcontroller; the board
/// advances it as frames come).
static NOW_NS: AtomicU64 = AtomicU64::new(0);

/// The platform clock (`styx_runtime::Clock`).
pub fn clock() -> Duration {
    Duration::from_nanos(NOW_NS.load(Ordering::Relaxed))
}

fn set_clock(ns: u64) {
    NOW_NS.store(ns, Ordering::Relaxed);
}

/// The sensor's registers in a fixed array, written by the driver as an I²C bus would.
pub struct Registers {
    values: Box<[u8; 65536]>,
    /// Register writes so far.
    pub writes: u64,
}

impl Registers {
    /// An OV9782 at power-on (its chip id answers).
    pub fn ov9782() -> Self {
        let mut values = Box::new([0u8; 65536]);
        values[0x300a] = 0x97;
        values[0x300b] = 0x82;
        Self { values, writes: 0 }
    }

    /// `bytes` bytes at `address`, big-endian.
    pub fn value(&self, address: u16, bytes: u8) -> u32 {
        (0..bytes).fold(0, |v, i| {
            v << 8 | u32::from(self.values[usize::from(address.wrapping_add(u16::from(i)))])
        })
    }
}

impl RegisterBus for Registers {
    type BusError = Infallible;

    fn read_burst(&mut self, address: u16, buf: &mut [u8]) -> RegisterResult<(), Infallible> {
        let mut at = address;
        for b in buf {
            *b = self.values[usize::from(at)];
            at = at.wrapping_add(1);
        }
        Ok(())
    }

    /// One register value (the driver writes one per call): counted.
    fn write_burst(&mut self, address: u16, data: &[u8]) -> RegisterResult<(), Infallible> {
        let mut at = address;
        for b in data {
            self.values[usize::from(at)] = *b;
            at = at.wrapping_add(1);
        }
        self.writes += 1;
        Ok(())
    }
}

impl DriverBus for Registers {}

/// Pins with no roles (the description's optional power steps are skipped), no waiting.
pub struct Pins;

impl DelayNs for Pins {
    fn delay_ns(&mut self, _ns: u32) {}
}

fn not_found(role: &str) -> styx_sensor::BusError {
    styx_sensor::BusError::new(styx_sensor::BusErrorKind::NotFound, role.to_string())
}

impl SensorPins for Pins {
    type Error = styx_sensor::BusError;
    fn set_gpio(&mut self, role: &str, _value: bool) -> BusResult<()> {
        Err(not_found(role))
    }
    fn set_clock(&mut self, role: &str, _rate_hz: Option<u32>) -> BusResult<()> {
        Err(not_found(role))
    }
    fn set_supply(&mut self, role: &str, _on: bool) -> BusResult<()> {
        Err(not_found(role))
    }
}

/// The sensor side on this board.
pub type Sensor = Lock<SensorState<Registers, Pins>>;

/// The OV9782 description, compiled into the firmware at build time.
pub fn description() -> Arc<SensorDescription> {
    Arc::new(
        SensorDescription::from_postcard(styx_sensor::include_description!("ov9782"))
            .expect("compiled description"),
    )
}

/// The OV9782 brought up in its 1280x800 RAW10 mode on the board's clock.
pub fn sensor() -> Shared<SensorState<Registers, Pins>> {
    let mut s = SensorState::new(SensorDriver::new(description(), Registers::ov9782(), Pins))
        .with_clock(clock);
    s.bring_up("1280x800", "raw10").expect("bring-up");
    styx_runtime::sync::shared(s)
}

/// One receiver buffer: memory the "DMA" (the board) writes while the buffer is queued.
pub struct ReplayBuffer {
    data: UnsafeCell<Box<[u8]>>,
}

// SAFETY: the receiver writes a buffer's bytes only while the buffer is queued to it (taken
// out of its queue under its lock, in `ReplayReceiver::expose`); the runtime reads them only
// through a lease, between a dequeue and the queue that ends it. The two never overlap, as
// with a DMA engine and a CPU.
unsafe impl Sync for ReplayBuffer {}

/// A receiver buffer handle (`Receiver::Buffer`).
#[derive(Clone)]
pub struct BufferHandle(Arc<ReplayBuffer>);

impl BufferHandle {
    fn bytes(&self) -> &[u8] {
        // SAFETY: see `ReplayBuffer`: no write happens while a lease reads.
        unsafe { &*self.0.data.get() }
    }
}

impl FrameBuffer for BufferHandle {
    type Export<'a> = usize;
    fn len(&self) -> usize {
        self.bytes().len()
    }
    fn bytes(&self) -> &[u8] {
        BufferHandle::bytes(self)
    }
    // Coherent memory: no cache maintenance.
    fn begin_cpu(&self, _access: Access) {}
    fn end_cpu(&self, _access: Access) {}
}

/// Coherent RAM the receiver owns: the defaults (cached, nothing to export).
impl styx_runtime::LeaseBuffer for BufferHandle {}

#[derive(Default)]
struct State {
    configured: u32,
    queued: VecDeque<u32>,
    sync: VecDeque<SyncEvent>,
    done: VecDeque<FrameDone>,
    sync_waker: Option<Waker>,
    done_waker: Option<Waker>,
    streaming: bool,
}

/// A receiver that "captures" what the board exposes into its own buffers: frame-start events
/// ([`Self::frame_start`]), then the frame into the oldest queued buffer ([`Self::expose`]),
/// or an overrun glitch when none is queued. [`StartOrder::ReceiverFirst`].
pub struct ReplayReceiver {
    buffers: Vec<Arc<ReplayBuffer>>,
    state: Mutex<State>,
}

impl ReplayReceiver {
    /// Up to `count` buffers of `len` bytes.
    pub fn new(count: u32, len: usize) -> Self {
        Self {
            buffers: (0..count)
                .map(|_| {
                    Arc::new(ReplayBuffer {
                        data: UnsafeCell::new(vec![0u8; len].into_boxed_slice()),
                    })
                })
                .collect(),
            state: Mutex::new(State::default()),
        }
    }

    fn push_sync(&self, event: SyncEvent) {
        let mut s = self.state.lock();
        s.sync.push_back(event);
        if let Some(w) = s.sync_waker.take() {
            w.wake();
        }
    }

    /// Frame `sequence` starts at `at` (a frame-start interrupt).
    pub fn frame_start(&self, sequence: u64, at: Instant) {
        self.push_sync(SyncEvent::FrameStart { sequence, at });
    }

    /// Frame `sequence` (started at `at`) is written by `fill` into the oldest queued buffer
    /// and delivered; with no buffer queued it is lost (an overrun). Returns the buffer used.
    pub fn expose(&self, sequence: u64, at: Instant, fill: impl FnOnce(&mut [u8])) -> Option<u32> {
        let index = {
            let mut s = self.state.lock();
            if !s.streaming {
                return None;
            }
            s.queued.pop_front()
        };
        let Some(index) = index else {
            self.push_sync(SyncEvent::Glitch(ErrorKind::Overrun));
            return None;
        };
        let buffer = &self.buffers[index as usize];
        // SAFETY: `index` was queued (the receiver's, no lease reads it); see `ReplayBuffer`.
        let data = unsafe { &mut *buffer.data.get() };
        fill(data);
        let mut s = self.state.lock();
        s.done.push_back(FrameDone {
            index,
            sequence,
            timestamp: at,
            bytes_used: data.len(),
            corrupt: false,
            stats_slot: None,
        });
        if let Some(w) = s.done_waker.take() {
            w.wake();
        }
        Some(index)
    }

    /// Buffers queued now.
    pub fn queued(&self) -> usize {
        self.state.lock().queued.len()
    }
}

impl Receiver for ReplayReceiver {
    type Error = ErrorKind;
    type Buffer = BufferHandle;

    fn caps(&self) -> ReceiverCaps {
        ReceiverCaps {
            frame_start_events: true,
            embedded_data: false,
            timestamp: TimestampPoint::FrameStart,
            start_order: StartOrder::ReceiverFirst,
            max_buffers: self.buffers.len() as u32,
        }
    }

    fn configure(&self, cfg: &ReceiverConfig) -> Result<Configured, ErrorKind> {
        let stride = cfg.stride.unwrap_or(cfg.width * 2);
        let buffer_len = stride as usize * cfg.height as usize;
        let room = self.buffers.first().map_or(0, |b| {
            // SAFETY: the length only; see `ReplayBuffer`.
            unsafe { &*b.data.get() }.len()
        });
        if cfg.buffers == 0 || cfg.buffers as usize > self.buffers.len() || buffer_len > room {
            return Err(ErrorKind::InvalidConfig);
        }
        let mut s = self.state.lock();
        s.configured = cfg.buffers;
        s.queued.clear();
        Ok(Configured {
            stride,
            buffer_len,
            buffers: cfg.buffers,
        })
    }

    fn buffer(&self, index: u32) -> Option<BufferHandle> {
        self.buffers
            .get(index as usize)
            .map(|b| BufferHandle(Arc::clone(b)))
    }

    fn queue(&self, index: u32) -> Result<(), ErrorKind> {
        let mut s = self.state.lock();
        if index >= s.configured || s.queued.contains(&index) {
            return Err(ErrorKind::InvalidConfig);
        }
        s.queued.push_back(index);
        Ok(())
    }

    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        self.state.lock().streaming = true;
        sensor.start()
    }

    fn stop<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        sensor.stop()?;
        let mut s = self.state.lock();
        s.streaming = false;
        s.queued.clear();
        Ok(())
    }

    fn release(&self) -> Result<(), ErrorKind> {
        let mut s = self.state.lock();
        s.configured = 0;
        s.queued.clear();
        Ok(())
    }

    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, ErrorKind>> {
        let mut s = self.state.lock();
        match s.sync.pop_front() {
            Some(e) => Poll::Ready(Ok(e)),
            None => {
                s.sync_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, ErrorKind>> {
        let mut s = self.state.lock();
        match s.done.pop_front() {
            Some(e) => Poll::Ready(Ok(e)),
            None => {
                s.done_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn try_sync(&self) -> Result<Option<SyncEvent>, ErrorKind> {
        Ok(self.state.lock().sync.pop_front())
    }

    fn try_done(&self) -> Result<Option<FrameDone>, ErrorKind> {
        Ok(self.state.lock().done.pop_front())
    }
}

/// The mock platform.
pub struct MockBoard;

impl Platform for MockBoard {
    type Receiver = ReplayReceiver;
    type Sensor = Sensor;
}

/// A raw recording held in memory (flash on a microcontroller): frames back to back and what
/// produced each.
#[derive(Clone, Debug)]
pub struct Recording {
    /// Geometry and levels.
    pub layout: Recorded,
    /// The frames, `layout.stride * layout.height` bytes each.
    pub data: Vec<u8>,
    /// What produced each frame.
    pub values: Vec<SensorValues>,
}

impl Recording {
    /// Frame `i` (cycling).
    pub fn frame(&self, i: usize) -> (&[u8], &SensorValues) {
        let i = i % self.values.len().max(1);
        let len = self.layout.stride * self.layout.height;
        (&self.data[i * len..(i + 1) * len], &self.values[i])
    }
}

/// The receiver configuration for frames of `rec` (16-bit samples, one row per stride).
pub fn receiver_config(rec: &Recording, buffers: u32) -> ReceiverConfig {
    ReceiverConfig {
        bus: Bus::Other,
        bus_code: 0x300f,
        fourcc: *b"BG10",
        width: rec.layout.width as u32,
        height: rec.layout.height as u32,
        stride: Some(rec.layout.width as u32 * 2),
        buffers,
        memory: BufferSource::Own,
        embedded: None,
        frame_starts: true,
    }
}

/// The scene in front of the board's camera: a recording replayed at whatever exposure and
/// gain the sensor's registers give each frame (as the control schedule recorded writing
/// them), at the sensor's frame rate on the board clock.
pub struct Board {
    /// The receiver (shared with the camera).
    pub receiver: Ref<ReplayReceiver>,
    /// The sensor (shared with the camera).
    pub sensor: Shared<SensorState<Registers, Pins>>,
    rec: Recording,
    next: u64,
    period_ns: u64,
    row: Vec<u16>,
}

impl Board {
    /// A board replaying `rec` with `buffers` receiver buffers, frames every `period`.
    pub fn new(rec: Recording, buffers: u32, period: Duration) -> Self {
        let len = rec.layout.width * 2 * rec.layout.height;
        set_clock(1_000_000_000);
        Self {
            receiver: Ref::new(ReplayReceiver::new(buffers, len)),
            sensor: sensor(),
            row: vec![0; rec.layout.width],
            rec,
            next: 0,
            period_ns: period.as_nanos() as u64,
        }
    }

    /// When frame `seq` starts on the board clock.
    fn start_ns(&self, seq: u64) -> u64 {
        1_000_000_000 + seq * self.period_ns
    }

    /// The next frame starts (the frame-start interrupt).
    pub fn frame_start(&mut self) -> u64 {
        let seq = self.next;
        let at = self.start_ns(seq);
        set_clock(at);
        self.receiver.frame_start(seq, Instant(at));
        seq
    }

    /// The frame that started ends: exposed with what the sensor applied to it, written into
    /// a receiver buffer and delivered, 90% of a period after its start.
    pub fn frame_end(&mut self) -> Option<u32> {
        let seq = self.next;
        self.next += 1;
        let at = self.start_ns(seq);
        set_clock(at + self.period_ns * 9 / 10);
        let applied = self.sensor.applied(seq)?;
        let values = sensor_values(seq, &applied);
        let (src, recorded) = self.rec.frame(seq as usize);
        let k = exposure_ratio(&values, recorded, 1.0);
        let (layout, row) = (&self.rec.layout, &mut self.row);
        self.receiver
            .expose(seq, Instant(at), |out| re_expose(src, layout, k, out, row))
    }

    /// Frames started so far.
    pub fn frames(&self) -> u64 {
        self.next
    }

    /// The register writes so far.
    pub fn register_writes(&self) -> u64 {
        lock(&self.sensor).driver().bus().writes
    }
}

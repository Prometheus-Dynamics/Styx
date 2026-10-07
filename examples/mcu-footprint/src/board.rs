//! The mock platform, kept small so that it does not count against Styx: a receiver that
//! "captures" a synthetic scene into buffers it owns, the OV9782 as a register bus that answers
//! its chip id, pins without roles, and the platform clock. A real port replaces this file with
//! its DCMI / CSI receiver, I²C bus, GPIOs and timer; the configurations above it stay.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::{RefCell, UnsafeCell};
use core::convert::Infallible;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use critical_section::Mutex;
use portable_atomic::{AtomicU64, Ordering};
use styx_core::sync::Arc;
use styx_hal::embedded_hal::delay::DelayNs;
use styx_hal::{
    Access, BufferSource, Bus, Configured, ErrorKind, FrameBuffer, FrameDone, Instant, Receiver,
    ReceiverCaps, ReceiverConfig, SensorStart, StartOrder, SyncEvent, TimestampPoint,
};
use styx_sensor::lemnos_hal::register::RegisterResult;
use styx_sensor::{BusResult, DriverBus, RegisterBus, SensorPins};

/// The platform clock in nanoseconds (a hardware timer on a microcontroller).
static NOW_NS: AtomicU64 = AtomicU64::new(0);

/// The platform clock (`styx_runtime::Clock`).
pub fn clock() -> Duration {
    Duration::from_nanos(NOW_NS.load(Ordering::Relaxed))
}

/// Sets the platform clock.
pub fn set_clock(ns: u64) {
    NOW_NS.store(ns, Ordering::Relaxed);
}

/// A frame buffer of `len` bytes: allocated with [`crate::frame_memory`] so the heap
/// measurements tell frame buffers from everything else.
pub fn frame_buffer(len: usize) -> Box<[u8]> {
    crate::frame_memory(|| alloc::vec![0u8; len].into_boxed_slice())
}

/// The sensor on its bus: answers the OV9782's chip id, counts writes, keeps nothing else (the
/// runtime predicts what each frame got from the control schedule).
#[derive(Default)]
pub struct Registers {
    /// Register writes so far.
    pub writes: u32,
}

impl RegisterBus for Registers {
    type BusError = Infallible;

    fn read_burst(&mut self, address: u16, buf: &mut [u8]) -> RegisterResult<(), Infallible> {
        let mut at = address;
        for b in buf {
            *b = match at {
                0x300a => 0x97,
                0x300b => 0x82,
                _ => 0,
            };
            at = at.wrapping_add(1);
        }
        Ok(())
    }

    /// One register value (the driver writes one per call): counted.
    fn write_burst(&mut self, _address: u16, _data: &[u8]) -> RegisterResult<(), Infallible> {
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

/// One receiver buffer: memory the "DMA" (the board) writes while the buffer is queued.
pub struct DmaBuffer {
    data: UnsafeCell<Box<[u8]>>,
}

// SAFETY: the receiver writes a buffer's bytes only while the buffer is queued to it (taken
// out of its queue under its lock, in `Dcmi::capture`); the runtime reads them only through a
// lease, between a dequeue and the queue that ends it. The two never overlap, as with a DMA
// engine and a CPU.
unsafe impl Sync for DmaBuffer {}

/// A receiver buffer handle (`Receiver::Buffer`).
#[derive(Clone)]
pub struct BufferHandle(Arc<DmaBuffer>);

impl FrameBuffer for BufferHandle {
    type Export<'a> = usize;
    fn len(&self) -> usize {
        self.bytes().len()
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: see `DmaBuffer`: no write happens while a lease reads.
        unsafe { &*self.0.data.get() }
    }
    // Coherent memory: no cache maintenance.
    fn begin_cpu(&self, _access: Access) {}
    fn end_cpu(&self, _access: Access) {}
}

/// Coherent RAM the receiver owns: the defaults (cached, nothing to export).
impl styx_runtime::LeaseBuffer for BufferHandle {}

/// A small ring of indices or events (no `VecDeque`: fixed, no allocation).
struct Ring<T: Copy, const N: usize> {
    items: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Ring<T, N> {
    const fn new() -> Self {
        Self {
            items: [None; N],
            head: 0,
            len: 0,
        }
    }
    fn push(&mut self, v: T) -> bool {
        if self.len == N {
            return false;
        }
        self.items[(self.head + self.len) % N] = Some(v);
        self.len += 1;
        true
    }
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.items[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        v
    }
    fn contains(&self, v: T) -> bool
    where
        T: PartialEq,
    {
        (0..self.len).any(|i| self.items[(self.head + i) % N] == Some(v))
    }
    fn clear(&mut self) {
        *self = Self::new();
    }
}

struct State {
    configured: u32,
    queued: Ring<u32, 4>,
    sync: Ring<SyncEvent, 4>,
    done: Ring<FrameDone, 4>,
    sync_waker: Option<Waker>,
    done_waker: Option<Waker>,
    streaming: bool,
}

/// A DCMI-like receiver: frame-start events ([`Self::frame_start`]), then the frame written into
/// the oldest queued buffer ([`Self::capture`]), or an overrun glitch when none is queued.
pub struct Dcmi {
    buffers: Vec<Arc<DmaBuffer>>,
    state: Mutex<RefCell<State>>,
}

impl Dcmi {
    /// Up to `count` (at most 4) buffers of `len` bytes.
    pub fn new(count: u32, len: usize) -> Self {
        Self {
            buffers: (0..count.min(4))
                .map(|_| {
                    Arc::new(DmaBuffer {
                        data: UnsafeCell::new(frame_buffer(len)),
                    })
                })
                .collect(),
            state: Mutex::new(RefCell::new(State {
                configured: 0,
                queued: Ring::new(),
                sync: Ring::new(),
                done: Ring::new(),
                sync_waker: None,
                done_waker: None,
                streaming: false,
            })),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        critical_section::with(|cs| f(&mut self.state.borrow_ref_mut(cs)))
    }

    /// Frame `sequence` starts at `at` (the frame-start interrupt).
    pub fn frame_start(&self, sequence: u64, at: Instant) {
        let waker = self.with(|s| {
            s.sync.push(SyncEvent::FrameStart { sequence, at });
            s.sync_waker.take()
        });
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Frame `sequence` (started at `at`) is written by `fill` into the oldest queued buffer
    /// and delivered (the DMA-complete interrupt); with no buffer queued it is lost.
    pub fn capture(&self, sequence: u64, at: Instant, fill: impl FnOnce(&mut [u8])) -> bool {
        let Some(index) = self.with(|s| if s.streaming { s.queued.pop() } else { None }) else {
            self.with(|s| s.sync.push(SyncEvent::Glitch(ErrorKind::Overrun)));
            return false;
        };
        // SAFETY: `index` was queued (the receiver's, no lease reads it); see `DmaBuffer`.
        let data = unsafe { &mut *self.buffers[index as usize].data.get() };
        fill(data);
        let bytes_used = data.len();
        let waker = self.with(|s| {
            s.done.push(FrameDone {
                index,
                sequence,
                timestamp: at,
                bytes_used,
                corrupt: false,
                stats_slot: None,
            });
            s.done_waker.take()
        });
        if let Some(w) = waker {
            w.wake();
        }
        true
    }
}

impl Receiver for Dcmi {
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
        let room = self
            .buffers
            .first()
            .map_or(0, |b| BufferHandle(b.clone()).len());
        if cfg.buffers == 0 || cfg.buffers as usize > self.buffers.len() || buffer_len > room {
            return Err(ErrorKind::InvalidConfig);
        }
        self.with(|s| {
            s.configured = cfg.buffers;
            s.queued.clear();
        });
        Ok(Configured {
            stride,
            buffer_len,
            buffers: cfg.buffers,
        })
    }

    fn buffer(&self, index: u32) -> Option<BufferHandle> {
        self.buffers
            .get(index as usize)
            .map(|b| BufferHandle(b.clone()))
    }

    fn queue(&self, index: u32) -> Result<(), ErrorKind> {
        self.with(|s| {
            if index >= s.configured || s.queued.contains(index) || !s.queued.push(index) {
                return Err(ErrorKind::InvalidConfig);
            }
            Ok(())
        })
    }

    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        self.with(|s| s.streaming = true);
        sensor.start()
    }

    fn stop<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        sensor.stop()?;
        self.with(|s| {
            s.streaming = false;
            s.queued.clear();
        });
        Ok(())
    }

    fn release(&self) -> Result<(), ErrorKind> {
        self.with(|s| {
            s.configured = 0;
            s.queued.clear();
        });
        Ok(())
    }

    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, ErrorKind>> {
        self.with(|s| match s.sync.pop() {
            Some(e) => Poll::Ready(Ok(e)),
            None => {
                s.sync_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, ErrorKind>> {
        self.with(|s| match s.done.pop() {
            Some(e) => Poll::Ready(Ok(e)),
            None => {
                s.done_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    fn try_sync(&self) -> Result<Option<SyncEvent>, ErrorKind> {
        Ok(self.with(|s| s.sync.pop()))
    }

    fn try_done(&self) -> Result<Option<FrameDone>, ErrorKind> {
        Ok(self.with(|s| s.done.pop()))
    }
}

/// The receiver configuration for `width` x `height` RAW10 frames in 16-bit samples.
pub fn receiver_config(width: u32, height: u32, buffers: u32) -> ReceiverConfig {
    ReceiverConfig {
        bus: Bus::Other,
        bus_code: 0x300f,
        fourcc: *b"BG10",
        width,
        height,
        stride: Some(width * 2),
        buffers,
        memory: BufferSource::Own,
        embedded: None,
        frame_starts: true,
    }
}

/// The scene: a BGGR mosaic of a warm-lit horizontal ramp (R 1.0, G 0.8, B 0.45 of a grey's
/// response) at `exposure_us` x `gain` (10 ms x 1 gives mid grey), 10-bit samples over a black
/// level of 64, little-endian 16-bit.
pub fn expose(out: &mut [u8], width: usize, exposure_us: u32, gain_q8: u32) {
    let scale = u64::from(exposure_us) * u64::from(gain_q8);
    for (y, row) in out.chunks_exact_mut(width * 2).enumerate() {
        for (x, px) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let k: u64 = match (y & 1, x & 1) {
                (0, 0) => 45,
                (1, 1) => 100,
                _ => 80,
            };
            let level = (40 + 240 * x / width) as u64;
            // level at 10 ms x 1 (scale 10_000 * 256), times the channel's response.
            let v = (64 + level * k * scale / (100 * 10_000 * 256)).min(1023) as u16;
            px.copy_from_slice(&v.to_le_bytes());
        }
    }
}

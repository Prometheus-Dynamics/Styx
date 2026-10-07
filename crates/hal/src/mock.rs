//! A mock platform for host tests (feature `mock`): output pins and a delay that record, a
//! receiver with injectable events and faults, heap-backed DMA memory, and a tiny [`block_on`]
//! for async tests. The I²C sensor model is Lemnos's ([`MockI2c`]: register-file targets,
//! blocking and async, shared clones, suspending async transfers, dead targets), re-exported.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::Duration;

pub use lemnos_hal::mock::{I2cTransfer, MockI2c, MockI2cTarget, MockOp};

use crate::dma::{Access, DmaBuffer, DmaMemory, FrameBuffer, Region};
use crate::error::ErrorKind;
use crate::receiver::{
    Configured, FrameDone, Receiver, ReceiverCaps, ReceiverConfig, SensorStart, StartOrder,
    SyncEvent, TimestampPoint,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a future to completion on the calling thread (parks between wake-ups). For tests: the
/// library has no executor.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// Returns `Pending` `n` times (waking itself each time), then `Ready`: makes an async mock
/// really suspend, so callers are tested across await points.
async fn yield_times(n: u32) {
    let mut left = n;
    core::future::poll_fn(|cx| {
        if left == 0 {
            Poll::Ready(())
        } else {
            left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

/// A recording output pin (shared log of levels).
#[derive(Clone, Debug, Default)]
pub struct MockPin {
    /// Every level set, in order (`true`: high).
    pub levels: Arc<Mutex<Vec<bool>>>,
}

impl MockPin {
    /// The levels set so far.
    pub fn history(&self) -> Vec<bool> {
        lock(&self.levels).clone()
    }
}

impl embedded_hal::digital::ErrorType for MockPin {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for MockPin {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        lock(&self.levels).push(false);
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Self::Error> {
        lock(&self.levels).push(true);
        Ok(())
    }
}

/// A delay that records and does not sleep (blocking and async).
#[derive(Clone, Debug, Default)]
pub struct MockDelay {
    /// Every wait, in order.
    pub waits: Arc<Mutex<Vec<Duration>>>,
}

impl MockDelay {
    /// The waits so far.
    pub fn history(&self) -> Vec<Duration> {
        lock(&self.waits).clone()
    }
}

impl embedded_hal::delay::DelayNs for MockDelay {
    fn delay_ns(&mut self, ns: u32) {
        lock(&self.waits).push(Duration::from_nanos(u64::from(ns)));
    }
}

impl embedded_hal_async::delay::DelayNs for MockDelay {
    async fn delay_ns(&mut self, ns: u32) {
        yield_times(1).await;
        lock(&self.waits).push(Duration::from_nanos(u64::from(ns)));
    }
}

/// A heap buffer that counts its cache maintenance calls.
#[derive(Debug)]
pub struct MockBuffer {
    data: Vec<u8>,
    syncs: Mutex<(u32, u32)>,
}

impl MockBuffer {
    /// `(begin_cpu, end_cpu)` calls so far.
    pub fn syncs(&self) -> (u32, u32) {
        *lock(&self.syncs)
    }
}

impl DmaBuffer for MockBuffer {
    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl FrameBuffer for MockBuffer {
    type Export<'a> = usize;
    fn len(&self) -> usize {
        self.data.len()
    }
    fn bytes(&self) -> &[u8] {
        &self.data
    }
    fn begin_cpu(&self, _access: Access) {
        lock(&self.syncs).0 += 1;
    }
    fn end_cpu(&self, _access: Access) {
        lock(&self.syncs).1 += 1;
    }
    fn export(&self) -> Option<usize> {
        Some(self.data.as_ptr() as usize)
    }
}

impl FrameBuffer for Arc<MockBuffer> {
    type Export<'a> = usize;
    fn len(&self) -> usize {
        self.data.len()
    }
    fn bytes(&self) -> &[u8] {
        &self.data
    }
    fn begin_cpu(&self, access: Access) {
        MockBuffer::begin_cpu(self, access);
    }
    fn end_cpu(&self, access: Access) {
        MockBuffer::end_cpu(self, access);
    }
    fn export(&self) -> Option<usize> {
        Some(self.data.as_ptr() as usize)
    }
}

/// Heap-backed [`DmaMemory`] with an optional limit (then [`ErrorKind::NoMemory`]).
#[derive(Debug, Default)]
pub struct MockMemory {
    /// Bytes left before allocations fail (`None`: no limit).
    pub limit: Option<usize>,
}

impl DmaMemory for MockMemory {
    type Buffer = MockBuffer;
    type Error = ErrorKind;
    fn allocate(
        &mut self,
        len: usize,
        _align: usize,
        _region: Region,
    ) -> Result<MockBuffer, ErrorKind> {
        if let Some(left) = &mut self.limit {
            *left = left.checked_sub(len).ok_or(ErrorKind::NoMemory)?;
        }
        Ok(MockBuffer {
            data: vec![0; len],
            syncs: Mutex::new((0, 0)),
        })
    }
}

#[derive(Debug, Default)]
struct ReceiverState {
    configured: u32,
    queued: VecDeque<u32>,
    sync: VecDeque<Result<SyncEvent, ErrorKind>>,
    done: VecDeque<Result<FrameDone, ErrorKind>>,
    sync_waker: Option<Waker>,
    done_waker: Option<Waker>,
    streaming: bool,
    fail_start: Option<ErrorKind>,
}

/// A receiver whose events the test injects: [`Self::frame`] takes the oldest queued buffer
/// and delivers a frame start and that buffer as filled, [`Self::push_sync`] /
/// [`Self::push_done`] inject anything (errors included), [`Self::fail_start`] makes the next
/// start fail. [`StartOrder::ReceiverFirst`]: `start` arms, then starts the sensor. Buffer
/// contents are not written (zeros).
#[derive(Debug)]
pub struct MockReceiver {
    buffers: Vec<Arc<MockBuffer>>,
    state: Mutex<ReceiverState>,
    sequence: Mutex<u64>,
}

impl MockReceiver {
    /// A receiver with up to `max_buffers` buffers of `buffer_len` bytes.
    pub fn new(max_buffers: u32, buffer_len: usize) -> Self {
        let mut memory = MockMemory::default();
        let buffers = (0..max_buffers)
            .map(|_| memory.allocate(buffer_len, 64, Region::Any).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default();
        Self {
            buffers,
            state: Mutex::default(),
            sequence: Mutex::default(),
        }
    }

    /// The next start fails with `kind`.
    pub fn fail_start(&self, kind: ErrorKind) {
        lock(&self.state).fail_start = Some(kind);
    }

    /// Injects a sync event.
    pub fn push_sync(&self, event: Result<SyncEvent, ErrorKind>) {
        let mut s = lock(&self.state);
        s.sync.push_back(event);
        if let Some(w) = s.sync_waker.take() {
            w.wake();
        }
    }

    /// Injects a filled-buffer event.
    pub fn push_done(&self, event: Result<FrameDone, ErrorKind>) {
        let mut s = lock(&self.state);
        s.done.push_back(event);
        if let Some(w) = s.done_waker.take() {
            w.wake();
        }
    }

    /// A frame at `at_ns`: a frame start and, if a buffer is queued, that buffer as filled
    /// (else an overrun glitch). Returns the buffer index used.
    pub fn frame(&self, at_ns: u64) -> Option<u32> {
        let sequence = {
            let mut s = lock(&self.sequence);
            let v = *s;
            *s += 1;
            v
        };
        let at = crate::Instant(at_ns);
        self.push_sync(Ok(SyncEvent::FrameStart { sequence, at }));
        let index = lock(&self.state).queued.pop_front();
        match index {
            Some(index) => self.push_done(Ok(FrameDone {
                index,
                sequence,
                timestamp: at,
                bytes_used: self.buffers[index as usize].len(),
                corrupt: false,
                stats_slot: None,
            })),
            None => self.push_sync(Ok(SyncEvent::Glitch(ErrorKind::Overrun))),
        }
        index
    }

    /// Buffers queued to the hardware, oldest first.
    pub fn queued(&self) -> Vec<u32> {
        lock(&self.state).queued.iter().copied().collect()
    }

    /// Whether it streams.
    pub fn streaming(&self) -> bool {
        lock(&self.state).streaming
    }
}

impl Receiver for MockReceiver {
    type Error = ErrorKind;
    type Buffer = Arc<MockBuffer>;

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
        let room = self.buffers.first().map_or(0, |b| b.len());
        if cfg.buffers == 0 || cfg.buffers as usize > self.buffers.len() || buffer_len > room {
            return Err(ErrorKind::InvalidConfig);
        }
        let mut s = lock(&self.state);
        s.configured = cfg.buffers;
        s.queued.clear();
        Ok(Configured {
            stride,
            buffer_len,
            buffers: cfg.buffers,
        })
    }

    fn buffer(&self, index: u32) -> Option<Arc<MockBuffer>> {
        self.buffers.get(index as usize).cloned()
    }

    fn queue(&self, index: u32) -> Result<(), ErrorKind> {
        let mut s = lock(&self.state);
        if index >= s.configured || s.queued.contains(&index) {
            return Err(ErrorKind::InvalidConfig);
        }
        s.queued.push_back(index);
        Ok(())
    }

    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        if let Some(kind) = lock(&self.state).fail_start.take() {
            return Err(kind);
        }
        lock(&self.state).streaming = true;
        sensor.start()
    }

    fn stop<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), ErrorKind> {
        sensor.stop()?;
        let mut s = lock(&self.state);
        s.streaming = false;
        s.queued.clear();
        Ok(())
    }

    fn release(&self) -> Result<(), ErrorKind> {
        let mut s = lock(&self.state);
        s.configured = 0;
        s.queued.clear();
        Ok(())
    }

    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, ErrorKind>> {
        let mut s = lock(&self.state);
        match s.sync.pop_front() {
            Some(e) => Poll::Ready(e),
            None => {
                s.sync_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, ErrorKind>> {
        let mut s = lock(&self.state);
        match s.done.pop_front() {
            Some(e) => Poll::Ready(e),
            None => {
                s.done_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn try_sync(&self) -> Result<Option<SyncEvent>, ErrorKind> {
        lock(&self.state).sync.pop_front().transpose()
    }

    fn try_done(&self) -> Result<Option<FrameDone>, ErrorKind> {
        lock(&self.state).done.pop_front().transpose()
    }
}

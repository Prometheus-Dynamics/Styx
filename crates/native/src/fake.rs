//! Fakes of the sensor bridge and a vb2 capture queue that follow the kernel's rules closely
//! enough to test the runtime's error paths on the host, with knobs that inject faults.
//!
//! - [`FakeBridge`] (`fake_bridge.rs`): stream requests with a bounded wait for the
//!   acknowledgement, power refused while not idle, and unplugging.
//! - [`FakeQueue`] and its [`FakeVideo`] handles (file descriptions): `REQBUFS` owned by the
//!   handle that allocated (`EBUSY` for others, and while streaming), orphaning on `REQBUFS 0`
//!   (memfd-backed buffers stay mapped), `QBUF` only of dequeued buffers, `STREAMON` calling
//!   the bridge, frames produced by [`FakeQueue::tick`] with frame-start events, corrupted
//!   frames, fatal queue errors and unplugging.
//!
//! Readiness goes through pipes (readable = a request, a frame or an event is pending; the
//! write end closed = hang-up), so the reactor waits on them as on the real nodes.

use std::collections::VecDeque;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use styx_graph::rt;
use styx_kernel::dma_heap::DmaBuf;
use styx_kernel::event::{Event, EventKind};
use styx_kernel::v4l2::{BufferFlags, DequeuedBuffer, Memory, QueueBuffer};
use styx_kernel::{Mapping, Wait};

use crate::device::CaptureDevice;
pub(crate) use crate::fake_bridge::{BridgeState, FakeBridge};

pub(crate) fn errno(e: i32) -> io::Error {
    io::Error::from_raw_os_error(e)
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A pipe used as a readiness flag with a count: one byte per pending item.
pub(crate) struct Signal {
    pub(crate) reader: PipeReader,
    writer: Mutex<Option<PipeWriter>>,
}

impl Signal {
    pub(crate) fn new() -> Self {
        let (reader, writer) = std::io::pipe().expect("pipe");
        rt::set_nonblocking(reader.as_fd()).expect("nonblocking");
        Self {
            reader,
            writer: Mutex::new(Some(writer)),
        }
    }

    pub(crate) fn raise(&self) {
        if let Some(w) = lock(&self.writer).as_mut() {
            let _ = w.write_all(&[1]);
        }
    }

    pub(crate) fn lower(&self) {
        let _ = (&self.reader).read(&mut [0u8; 1]);
    }

    pub(crate) fn clear(&self) {
        while (&self.reader).read(&mut [0u8; 64]).is_ok_and(|n| n > 0) {}
    }

    /// Closes the write end: the read end reports a hang-up.
    pub(crate) fn hang_up(&self) {
        lock(&self.writer).take();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BufState {
    Dequeued,
    Queued,
    Done,
}

struct FakeBuffer {
    state: BufState,
    /// The memory the queue writes into: its own (MMAP) or the imported dma-buf.
    memory: Option<OwnedFd>,
    len: usize,
    sequence: u32,
    error: bool,
}

struct QueueInner {
    buffers: Vec<FakeBuffer>,
    memory: Memory,
    owner: Option<u64>,
    streaming: bool,
    done: VecDeque<u32>,
    events: VecDeque<u32>,
    sequence: u32,
    // Knobs.
    frame_sync: bool,
    corrupt: bool,
    fatal: bool,
    gone: bool,
    // Counters.
    frames: u64,
    dropped: u64,
    bad_qbufs: u64,
    orphaned: u64,
    reqbufs_busy: u64,
}

/// What a [`FakeQueue`] counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueueCounters {
    /// Frames written into buffers.
    pub(crate) frames: u64,
    /// Frames dropped for lack of a queued buffer.
    pub(crate) dropped: u64,
    /// `QBUF`s refused.
    pub(crate) bad_qbufs: u64,
    /// Buffers orphaned by `REQBUFS`.
    pub(crate) orphaned: u64,
    /// `REQBUFS` refused with `EBUSY`.
    pub(crate) reqbufs_busy: u64,
}

/// A capture queue shared by the handles opened on it.
pub(crate) struct FakeQueue {
    /// The node's lock, as `rp1-cfe`'s: every ioctl takes it, and `STREAMON`/`STREAMOFF`
    /// hold it while the bridge waits for its acknowledgement.
    node: Mutex<()>,
    inner: Mutex<QueueInner>,
    bridge: Arc<FakeBridge>,
    frames: Signal,
    events: Signal,
    buffer_len: usize,
    next_file: AtomicU64,
}

/// One open handle (file description) of a [`FakeQueue`], with its own descriptors (as a
/// second `open` of a node has).
pub(crate) struct FakeVideo {
    queue: Arc<FakeQueue>,
    file: u64,
    frames_fd: OwnedFd,
    /// `None`: events are reported on `frames_fd`, as V4L2 nodes do (as priority data, which
    /// a pipe never signals: they are only seen when the thread wakes for something else).
    events_fd: Option<OwnedFd>,
}

impl FakeQueue {
    pub(crate) fn new(bridge: Arc<FakeBridge>, buffer_len: usize) -> Arc<Self> {
        Arc::new(Self {
            node: Mutex::new(()),
            inner: Mutex::new(QueueInner {
                buffers: Vec::new(),
                memory: Memory::Mmap,
                owner: None,
                streaming: false,
                done: VecDeque::new(),
                events: VecDeque::new(),
                sequence: 0,
                frame_sync: true,
                corrupt: false,
                fatal: false,
                gone: false,
                frames: 0,
                dropped: 0,
                bad_qbufs: 0,
                orphaned: 0,
                reqbufs_busy: 0,
            }),
            bridge,
            frames: Signal::new(),
            events: Signal::new(),
            buffer_len,
            next_file: AtomicU64::new(1),
        })
    }

    /// Opens a handle.
    pub(crate) fn open(self: &Arc<Self>) -> Arc<FakeVideo> {
        self.open_with(false)
    }

    /// Opens a handle that reports events on its frame descriptor, like a V4L2 node.
    pub(crate) fn open_shared(self: &Arc<Self>) -> Arc<FakeVideo> {
        self.open_with(true)
    }

    fn open_with(self: &Arc<Self>, shared: bool) -> Arc<FakeVideo> {
        let dup = |fd: BorrowedFd<'_>| fd.try_clone_to_owned().expect("dup");
        Arc::new(FakeVideo {
            queue: Arc::clone(self),
            file: self.next_file.fetch_add(1, Ordering::Relaxed),
            frames_fd: dup(self.frames.reader.as_fd()),
            events_fd: (!shared).then(|| dup(self.events.reader.as_fd())),
        })
    }

    /// One frame period: a frame-start event (if enabled), then the frame into the first
    /// queued buffer (or dropped without one). Returns the sequence, if streaming.
    pub(crate) fn tick(&self) -> Option<u32> {
        let mut q = lock(&self.inner);
        if !q.streaming || q.gone {
            return None;
        }
        let seq = q.sequence;
        q.sequence += 1;
        if q.frame_sync {
            q.events.push_back(seq);
            self.events.raise();
        }
        let corrupt = q.corrupt;
        let Some(i) = q.buffers.iter().position(|b| b.state == BufState::Queued) else {
            q.dropped += 1;
            return Some(seq);
        };
        let b = &mut q.buffers[i];
        if let Some(fd) = &b.memory
            && let Ok(dup) = fd.try_clone()
            && let Ok(mut map) = DmaBuf::from_fd(dup, b.len).map()
        {
            fill(map.as_mut_slice(), seq);
        }
        b.state = BufState::Done;
        b.sequence = seq;
        b.error = corrupt;
        q.done.push_back(i as u32);
        q.frames += 1;
        self.frames.raise();
        Some(seq)
    }

    /// Runs [`Self::tick`] every `period` on a thread until the returned guard is dropped.
    pub(crate) fn run(self: &Arc<Self>, period: Duration) -> Producer {
        let stop = Arc::new(AtomicBool::new(false));
        let (q, s) = (Arc::clone(self), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                q.tick();
                std::thread::sleep(period);
            }
        });
        Producer {
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn set_frame_sync(&self, on: bool) {
        lock(&self.inner).frame_sync = on;
    }

    /// Frames from now on are flagged as corrupted.
    pub(crate) fn set_corrupt(&self, on: bool) {
        lock(&self.inner).corrupt = on;
    }

    /// The queue enters the error state (`vb2_queue_error`): `DQBUF` fails with `EIO`.
    pub(crate) fn fail(&self) {
        lock(&self.inner).fatal = true;
        self.frames.raise();
    }

    /// The node goes away (the receiver unbinds): calls fail with `ENODEV`, waits hang up,
    /// and a running stream is stopped, which sends the bridge its stop request.
    pub(crate) fn unplug(&self) {
        let mut q = lock(&self.inner);
        q.gone = true;
        let was_streaming = std::mem::replace(&mut q.streaming, false);
        drop(q);
        self.frames.hang_up();
        self.events.hang_up();
        if was_streaming {
            let bridge = Arc::clone(&self.bridge);
            std::thread::spawn(move || bridge.s_stream(false));
        }
    }

    pub(crate) fn num_buffers(&self) -> usize {
        lock(&self.inner).buffers.len()
    }

    pub(crate) fn is_streaming(&self) -> bool {
        lock(&self.inner).streaming
    }

    pub(crate) fn counters(&self) -> QueueCounters {
        let q = lock(&self.inner);
        QueueCounters {
            frames: q.frames,
            dropped: q.dropped,
            bad_qbufs: q.bad_qbufs,
            orphaned: q.orphaned,
            reqbufs_busy: q.reqbufs_busy,
        }
    }

    /// Buffers owned by the driver right now (queued or done).
    pub(crate) fn queued(&self) -> usize {
        lock(&self.inner)
            .buffers
            .iter()
            .filter(|b| b.state != BufState::Dequeued)
            .count()
    }
}

/// Fills a frame with a pattern of its sequence.
pub(crate) fn fill(data: &mut [u8], seq: u32) {
    data.fill(seq as u8);
    if data.len() >= 4 {
        data[..4].copy_from_slice(&seq.to_le_bytes());
    }
}

/// The sequence a filled frame carries, if the pattern is intact.
pub(crate) fn pattern_of(data: &[u8]) -> Option<u32> {
    let seq = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    data[4..].iter().all(|&b| b == seq as u8).then_some(seq)
}

/// Stops a producer thread when dropped.
pub(crate) struct Producer {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl FakeVideo {
    fn check(&self, q: &QueueInner) -> io::Result<()> {
        if q.gone {
            Err(errno(libc::ENODEV))
        } else {
            Ok(())
        }
    }

    /// Streaming off: every buffer back to userspace, pending frames and events gone.
    fn cancel(&self) {
        let mut q = lock(&self.queue.inner);
        q.streaming = false;
        q.done.clear();
        q.events.clear();
        q.fatal = false;
        for b in &mut q.buffers {
            b.state = BufState::Dequeued;
        }
        self.queue.frames.clear();
        self.queue.events.clear();
    }
}

impl AsFd for FakeVideo {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.frames_fd.as_fd()
    }
}

impl CaptureDevice for FakeVideo {
    fn request_buffers(&self, memory: Memory, count: u32) -> io::Result<u32> {
        let _node = lock(&self.queue.node);
        let mut q = lock(&self.queue.inner);
        self.check(&q)?;
        if q.streaming || q.owner.is_some_and(|o| o != self.file) {
            q.reqbufs_busy += 1;
            return Err(errno(libc::EBUSY));
        }
        // Orphan whatever exists: mappings and exports keep their memfds alive.
        q.orphaned += q.buffers.len() as u64;
        q.buffers.clear();
        q.done.clear();
        self.queue.frames.clear();
        q.memory = memory;
        if count == 0 {
            q.owner = None;
            return Ok(0);
        }
        let n = count.min(8);
        for _ in 0..n {
            let memory = match memory {
                Memory::Mmap => Some(
                    DmaBuf::memfd("fake-vb2", self.queue.buffer_len)
                        .map_err(io::Error::from)?
                        .into_fd(),
                ),
                _ => None,
            };
            q.buffers.push(FakeBuffer {
                state: BufState::Dequeued,
                memory,
                len: self.queue.buffer_len,
                sequence: 0,
                error: false,
            });
        }
        q.owner = Some(self.file);
        Ok(n)
    }

    fn map_buffer(&self, index: u32) -> io::Result<Mapping> {
        let _node = lock(&self.queue.node);
        let q = lock(&self.queue.inner);
        self.check(&q)?;
        let b = q
            .buffers
            .get(index as usize)
            .ok_or_else(|| errno(libc::EINVAL))?;
        let fd = b.memory.as_ref().ok_or_else(|| errno(libc::EINVAL))?;
        Ok(DmaBuf::from_fd(fd.try_clone()?, b.len).map()?)
    }

    fn export_buffer(&self, index: u32) -> io::Result<OwnedFd> {
        let _node = lock(&self.queue.node);
        let q = lock(&self.queue.inner);
        self.check(&q)?;
        let b = q
            .buffers
            .get(index as usize)
            .ok_or_else(|| errno(libc::EINVAL))?;
        b.memory
            .as_ref()
            .ok_or_else(|| errno(libc::EINVAL))?
            .try_clone()
    }

    fn queue(&self, req: &QueueBuffer<'_>) -> io::Result<()> {
        let _node = lock(&self.queue.node);
        let mut q = lock(&self.queue.inner);
        self.check(&q)?;
        let memory = q.memory;
        let owner_ok = q.owner == Some(self.file);
        let valid = q
            .buffers
            .get(req.index as usize)
            .is_some_and(|b| b.state == BufState::Dequeued);
        if !owner_ok || !valid || req.memory != memory {
            q.bad_qbufs += 1;
            return Err(errno(if owner_ok { libc::EINVAL } else { libc::EBUSY }));
        }
        let b = &mut q.buffers[req.index as usize];
        if memory == Memory::DmaBuf {
            let fd = req
                .planes
                .first()
                .and_then(|p| p.dmabuf)
                .ok_or_else(|| errno(libc::EINVAL))?;
            b.memory = Some(fd.try_clone_to_owned()?);
        }
        b.state = BufState::Queued;
        Ok(())
    }

    fn dequeue(&self, _memory: Memory) -> io::Result<Option<DequeuedBuffer>> {
        let _node = lock(&self.queue.node);
        let mut q = lock(&self.queue.inner);
        self.check(&q)?;
        if q.fatal {
            return Err(errno(libc::EIO));
        }
        let Some(i) = q.done.pop_front() else {
            return Ok(None);
        };
        self.queue.frames.lower();
        let b = &mut q.buffers[i as usize];
        b.state = BufState::Dequeued;
        let mut flags = BufferFlags::empty();
        if b.error {
            flags |= BufferFlags::ERROR;
        }
        Ok(Some(DequeuedBuffer {
            index: i,
            sequence: b.sequence,
            flags,
            field: 1,
            timestamp: Duration::from_millis(u64::from(b.sequence) * 10),
            planes: vec![(b.len as u32, 0)],
        }))
    }

    fn stream_on(&self) -> io::Result<()> {
        let _node = lock(&self.queue.node);
        {
            let mut q = lock(&self.queue.inner);
            self.check(&q)?;
            if q.owner != Some(self.file) {
                return Err(errno(libc::EBUSY));
            }
            if q.streaming {
                return Ok(());
            }
            if q.buffers.is_empty() {
                return Err(errno(libc::EINVAL));
            }
            q.streaming = true;
            q.sequence = 0;
        }
        // As rp1-cfe: the receiver starts, then calls the sensor's s_stream(1) without the
        // queue lock.
        let r = self.queue.bridge.s_stream(true);
        if r.is_err() {
            self.cancel();
        }
        r
    }

    fn stream_off(&self) -> io::Result<()> {
        let _node = lock(&self.queue.node);
        {
            let q = lock(&self.queue.inner);
            self.check(&q)?;
            if q.owner.is_some_and(|o| o != self.file) {
                return Err(errno(libc::EBUSY));
            }
            if !q.streaming {
                drop(q);
                self.cancel();
                return Ok(());
            }
        }
        // A stop cannot fail for the receiver.
        let _ = self.queue.bridge.s_stream(false);
        self.cancel();
        Ok(())
    }

    fn dequeue_event(&self) -> io::Result<Option<Event>> {
        let _node = lock(&self.queue.node);
        let mut q = lock(&self.queue.inner);
        self.check(&q)?;
        let Some(seq) = q.events.pop_front() else {
            return Ok(None);
        };
        self.queue.events.lower();
        Ok(Some(Event {
            kind: EventKind::FrameSync {
                frame_sequence: seq,
            },
            pending: q.events.len() as u32,
            sequence: seq,
            timestamp: Duration::ZERO,
            id: 0,
        }))
    }

    fn event_fd(&self) -> BorrowedFd<'_> {
        self.events_fd.as_ref().unwrap_or(&self.frames_fd).as_fd()
    }

    fn event_wait(&self) -> Wait {
        if self.events_fd.is_some() {
            Wait::READABLE
        } else {
            Wait::PRIORITY
        }
    }
}

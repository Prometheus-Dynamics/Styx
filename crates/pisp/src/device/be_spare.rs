//! Spare output buffers, allocated off the capture thread.
//!
//! A cached-heap output that starts with fewer buffers than its node has slots grows into them
//! when every buffer is out. Allocating there delays that frame: 10-20 ms for contiguous memory
//! when it is short, 4-17 ms for the system heap with its mapping. So an output with slots left
//! keeps one spare: a helper thread ([`SpareGrower`]) allocates and maps it, and the capture
//! thread only adopts a ready spare (a push into the output's lists). It never waits for the
//! helper: when no spare is ready, the job is dropped as before ([`take_buffer`] counts it).
//!
//! The helper owns the heaps and the spare in flight, nothing else; the handoff is the two
//! channels. Dropping the grower ends the helper after its current allocation and joins it.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use styx_kernel::Mapping;
use styx_kernel::dma_heap::{DmaBuf, DmaHeap};

use super::{DeviceError, Result, profile};

/// Allocates cached dma-bufs for an output: the dma-heaps, or memfds in tests.
pub(super) trait Allocate: Send + 'static {
    /// A buffer of `len` bytes; `system_first`: the system heap before contiguous memory.
    fn allocate(&mut self, len: usize, system_first: bool) -> Result<DmaBuf>;
}

/// Allocates from the cached dma-heaps in order: contiguous memory (`linux,cma`), then the
/// system heap (the CM5 has 64 MiB of it, which many buffers of extra passes can exhaust).
pub(super) struct HeapAllocator {
    heaps: Vec<DmaHeap>,
    name: &'static str,
}

impl HeapAllocator {
    pub(super) fn new(heaps: Vec<DmaHeap>, name: &'static str) -> Self {
        Self { heaps, name }
    }
}

impl Allocate for HeapAllocator {
    fn allocate(&mut self, len: usize, system_first: bool) -> Result<DmaBuf> {
        let mut last = None;
        let mut order: Vec<&DmaHeap> = self.heaps.iter().collect();
        if system_first {
            order.reverse();
        }
        let buf = order.iter().find_map(|heap| match heap.allocate(len) {
            Ok(buf) => Some(buf),
            Err(e) => {
                last = Some(e);
                None
            }
        });
        match (buf, last) {
            (Some(buf), _) => Ok(buf),
            (None, Some(e)) => Err(e.into()),
            (None, None) => Err(DeviceError::Setup(format!("{}: no dma-heap", self.name))),
        }
    }
}

/// A buffer ready to adopt: mapped and exported.
pub(super) struct Spare {
    len: usize,
    map: Mapping,
    fd: OwnedFd,
}

impl Spare {
    /// Maps and exports `buf`.
    pub(super) fn new(buf: DmaBuf) -> Result<Self> {
        let len = buf.len();
        let map = buf.map()?;
        Ok(Self {
            len,
            map,
            fd: buf.into_fd(),
        })
    }

    /// The mapping and the dma-buf, for the output's lists.
    pub(super) fn into_parts(self) -> (Mapping, OwnedFd) {
        (self.map, self.fd)
    }
}

/// The helper thread of one output and the handoff with it. At most one spare is in flight or
/// ready at a time.
pub(super) struct SpareGrower {
    /// Requests to the helper; dropped to stop it.
    jobs: Option<Sender<()>>,
    /// Results from the helper, one per request.
    results: Receiver<Result<Spare>>,
    helper: Option<JoinHandle<()>>,
    /// A request was sent and its result has not arrived.
    requested: bool,
    /// A result that arrived and is waiting to be adopted.
    ready: Option<Spare>,
}

impl SpareGrower {
    /// Starts the helper thread, which allocates `len`-byte spares from `alloc` on request.
    pub(super) fn spawn<A: Allocate>(mut alloc: A, len: usize, name: &'static str) -> Result<Self> {
        let (jobs, job_rx) = mpsc::channel::<()>();
        let (result_tx, results) = mpsc::channel();
        let helper = thread::Builder::new()
            .name(format!("pisp-grow-{name}"))
            .spawn(move || {
                for _ in job_rx {
                    let spare = profile::time(name, "grow", || {
                        alloc.allocate(len, true).and_then(Spare::new)
                    });
                    // The grower was dropped: the spare goes with it.
                    if result_tx.send(spare).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| DeviceError::Setup(format!("{name}: spare thread: {e}")))?;
        Ok(Self {
            jobs: Some(jobs),
            results,
            helper: Some(helper),
            requested: false,
            ready: None,
        })
    }

    /// Moves a finished result to `ready`. A failed allocation is dropped: the next request
    /// tries again, and a job that needed the buffer counts a miss meanwhile.
    fn poll(&mut self) {
        if !self.requested {
            return;
        }
        match self.results.try_recv() {
            Ok(Ok(spare)) => {
                self.ready = Some(spare);
                self.requested = false;
            }
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => self.requested = false,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Asks for one more spare, unless one is ready or in flight. Never waits.
    pub(super) fn request(&mut self) {
        self.poll();
        if self.requested || self.ready.is_some() {
            return;
        }
        self.requested = self.jobs.as_ref().is_some_and(|j| j.send(()).is_ok());
    }

    /// The ready spare, if one arrived and is `len` bytes (a spare of another size, from before
    /// a format change, is dropped). Never waits.
    pub(super) fn take(&mut self, len: usize) -> Option<Spare> {
        self.poll();
        let spare = self.ready.take()?;
        (spare.len == len).then_some(spare)
    }
}

impl Drop for SpareGrower {
    fn drop(&mut self) {
        // Ends the helper after its current allocation (the result channel is unbounded, so it
        // never blocks on sending).
        self.jobs = None;
        if let Some(helper) = self.helper.take() {
            let _ = helper.join();
        }
    }
}

/// What an output's free list needs from its buffer slots: adopting a ready spare as a new
/// buffer, and asking for the next spare.
pub(super) trait Spares {
    /// Adopts a ready spare as a new buffer: its index.
    fn adopt(&mut self) -> Option<u32>;
    /// Asks for the next spare, when the output has slots left.
    fn request(&mut self);
}

/// A buffer for a job: a free one, else an adopted spare. `None` when neither is ready (the
/// job's frame is dropped; `misses` counts it). Keeps the next spare requested once the free
/// list is low.
pub(super) fn take_buffer(
    free: &mut VecDeque<u32>,
    spares: &mut impl Spares,
    misses: &mut u64,
) -> Option<u32> {
    let buffer = free.pop_front().or_else(|| spares.adopt());
    if buffer.is_none() {
        *misses += 1;
    }
    if free.len() <= 1 {
        spares.request();
    }
    buffer
}

/// Between frames: adopts a ready spare into an empty free list, and keeps the next spare
/// requested once the free list is low.
pub(super) fn top_up_free(free: &mut VecDeque<u32>, spares: &mut impl Spares) {
    if free.is_empty()
        && let Some(buffer) = spares.adopt()
    {
        free.push_back(buffer);
    }
    if free.len() <= 1 {
        spares.request();
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    /// Sets its flag when dropped (the helper's allocator is dropped when the helper ends).
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Memfd-backed buffers, counting allocations; optionally failing, slow (blocked on `gate`),
    /// or of the wrong size.
    struct Fake {
        allocs: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
        gate: Option<Receiver<()>>,
        extra: usize,
        dropped: DropFlag,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                allocs: Arc::default(),
                fail: Arc::default(),
                gate: None,
                extra: 0,
                dropped: DropFlag(Arc::default()),
            }
        }

        fn dropped(&self) -> Arc<AtomicBool> {
            self.dropped.0.clone()
        }
    }

    impl Allocate for Fake {
        fn allocate(&mut self, len: usize, _system_first: bool) -> Result<DmaBuf> {
            if let Some(gate) = &self.gate {
                let _ = gate.recv();
            }
            self.allocs.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(DeviceError::Setup("fake: out of memory".into()));
            }
            Ok(DmaBuf::memfd("spare-test", len + self.extra)?)
        }
    }

    const LEN: usize = 4096;

    /// Waits (up to 5 s) for a spare of `LEN` bytes; the test thread may wait, the capture
    /// thread never does.
    fn wait_take(g: &mut SpareGrower) -> Option<Spare> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = g.take(LEN) {
                return Some(s);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_requested_spare_is_adopted_once_ready() {
        let mut g = SpareGrower::spawn(Fake::new(), LEN, "t").unwrap();
        assert!(g.take(LEN).is_none(), "nothing requested yet");
        g.request();
        let spare = wait_take(&mut g).expect("spare");
        let (map, fd) = spare.into_parts();
        assert_eq!(map.len(), LEN);
        assert_eq!(
            styx_kernel::dma_heap::dmabuf_size(fd.as_fd()).unwrap(),
            LEN as u64
        );
        assert!(
            g.take(LEN).is_none(),
            "adopted spares are not handed out twice"
        );
    }

    #[test]
    fn take_never_waits_for_the_allocator() {
        let (open, gate) = mpsc::channel();
        let fake = Fake {
            gate: Some(gate),
            ..Fake::new()
        };
        let mut g = SpareGrower::spawn(fake, LEN, "t").unwrap();
        g.request();
        let start = Instant::now();
        assert!(g.take(LEN).is_none());
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "take must not block"
        );
        open.send(()).unwrap();
        assert!(wait_take(&mut g).is_some());
    }

    #[test]
    fn one_allocation_per_request() {
        let fake = Fake::new();
        let allocs = fake.allocs.clone();
        let mut g = SpareGrower::spawn(fake, LEN, "t").unwrap();
        g.request();
        g.request();
        g.request();
        let _ = wait_take(&mut g).expect("spare");
        assert_eq!(allocs.load(Ordering::SeqCst), 1);
        g.request();
        let _ = wait_take(&mut g).expect("second spare");
        assert_eq!(allocs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_allocation_is_not_adopted_and_is_retried() {
        let fake = Fake::new();
        let fail = fake.fail.clone();
        let mut g = SpareGrower::spawn(fake, LEN, "t").unwrap();
        fail.store(true, Ordering::SeqCst);
        g.request();
        thread::sleep(Duration::from_millis(50));
        assert!(g.take(LEN).is_none());
        fail.store(false, Ordering::SeqCst);
        g.request();
        assert!(wait_take(&mut g).is_some());
    }

    #[test]
    fn a_spare_of_another_size_is_dropped() {
        let fake = Fake {
            extra: LEN,
            ..Fake::new()
        };
        let mut g = SpareGrower::spawn(fake, LEN, "t").unwrap();
        g.request();
        thread::sleep(Duration::from_millis(50));
        assert!(g.take(LEN).is_none());
        assert!(g.take(2 * LEN).is_none(), "the discarded spare is gone");
    }

    #[test]
    fn dropping_the_grower_joins_the_helper() {
        let fake = Fake::new();
        let dropped = fake.dropped();
        let mut g = SpareGrower::spawn(fake, LEN, "t").unwrap();
        g.request();
        drop(g);
        assert!(
            dropped.load(Ordering::SeqCst),
            "the helper (and its allocator) ended first"
        );
    }

    #[test]
    fn dropping_an_idle_grower_joins_the_helper() {
        let fake = Fake::new();
        let dropped = fake.dropped();
        drop(SpareGrower::spawn(fake, LEN, "t").unwrap());
        assert!(dropped.load(Ordering::SeqCst));
    }

    /// Spares that are ready, and the requests made.
    #[derive(Default)]
    struct FakeSpares {
        ready: VecDeque<u32>,
        requests: usize,
    }

    impl Spares for FakeSpares {
        fn adopt(&mut self) -> Option<u32> {
            self.ready.pop_front()
        }

        fn request(&mut self) {
            self.requests += 1;
        }
    }

    #[test]
    fn take_prefers_free_buffers_and_keeps_a_spare_requested() {
        let mut free = VecDeque::from([0, 1, 2]);
        let mut spares = FakeSpares {
            ready: VecDeque::from([9]),
            ..Default::default()
        };
        let mut misses = 0;
        assert_eq!(take_buffer(&mut free, &mut spares, &mut misses), Some(0));
        assert_eq!(spares.requests, 0, "free list still has two");
        assert_eq!(take_buffer(&mut free, &mut spares, &mut misses), Some(1));
        assert_eq!(spares.requests, 1, "free list is down to one");
        assert_eq!(
            spares.ready.len(),
            1,
            "the spare is not adopted until needed"
        );
        assert_eq!(misses, 0);
    }

    #[test]
    fn take_adopts_a_ready_spare_when_no_buffer_is_free() {
        let mut free = VecDeque::new();
        let mut spares = FakeSpares {
            ready: VecDeque::from([7]),
            ..Default::default()
        };
        let mut misses = 0;
        assert_eq!(take_buffer(&mut free, &mut spares, &mut misses), Some(7));
        assert_eq!(misses, 0);
        assert!(spares.requests >= 1);
    }

    #[test]
    fn take_counts_a_miss_when_nothing_is_ready() {
        let mut free = VecDeque::new();
        let mut spares = FakeSpares::default();
        let mut misses = 0;
        assert_eq!(take_buffer(&mut free, &mut spares, &mut misses), None);
        assert_eq!(take_buffer(&mut free, &mut spares, &mut misses), None);
        assert_eq!(misses, 2);
        assert!(spares.requests >= 2, "each miss still asks for a spare");
    }

    #[test]
    fn top_up_adopts_only_into_an_empty_free_list() {
        let mut spares = FakeSpares {
            ready: VecDeque::from([5]),
            ..Default::default()
        };
        let mut free = VecDeque::from([1, 2]);
        top_up_free(&mut free, &mut spares);
        assert_eq!(free, VecDeque::from([1, 2]));
        assert_eq!(spares.ready.len(), 1);

        let mut free = VecDeque::new();
        top_up_free(&mut free, &mut spares);
        assert_eq!(free, VecDeque::from([5]));
        assert!(spares.requests >= 1);
    }
}

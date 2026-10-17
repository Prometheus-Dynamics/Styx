//! Sliced frames' progress, shared between a camera service and a client: one memfd page per
//! connection with a ring of slots, sent once with the first sliced frame. The service writes a
//! frame's rows and state into the frame's slot and wakes the slot's futex; the client's leases
//! read the slot ([`SlotSource`], a [`ProgressSource`]) on every query and wait on the futex.
//!
//! A slot is reused [`SLOTS`] frames later: a client reading a slot that names another frame
//! knows its own frame finished long ago (it completed before the service moved on).

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use styx_core::buffer::{ProgressSource, ProgressState};

/// Slots in a ring: frames in flight to one client at once, with room to spare.
pub(super) const SLOTS: usize = 16;
const PAGE: usize = 4096;

const RUNNING: u32 = 0;
const COMPLETE: u32 = 1;
const FAILED: u32 = 2;

/// One frame's progress, as both processes see it.
#[repr(C)]
struct Slot {
    /// Bumped on every change: the futex word.
    seq: AtomicU32,
    state: AtomicU32,
    rows: [AtomicU32; 2],
    /// The frame this slot is for (the frame message's id).
    id: AtomicU64,
}

const _: () = assert!(SLOTS * std::mem::size_of::<Slot>() <= PAGE);

/// A connection's progress page, mapped.
pub(super) struct ProgressRing {
    fd: OwnedFd,
    ptr: *mut libc::c_void,
}

// SAFETY: the mapping is only accessed through atomics; it lives as long as the ring.
unsafe impl Send for ProgressRing {}
// SAFETY: as above: every access is atomic.
unsafe impl Sync for ProgressRing {}

impl ProgressRing {
    /// A new page (the service side).
    pub(super) fn create() -> std::io::Result<Self> {
        // SAFETY: a fresh memfd; the result is checked.
        let raw = unsafe { libc::memfd_create(c"styx-progress".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor we just created and own.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        // SAFETY: sizing our own memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), PAGE as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Self::map(fd)
    }

    /// The page another process sent (the client side). Refused unless it is a page long.
    pub(super) fn from_fd(fd: OwnedFd) -> std::io::Result<Self> {
        // SAFETY: fstat fills the struct for a descriptor we own.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (st.st_size as usize) < PAGE {
            return Err(std::io::Error::other("progress page too short"));
        }
        Self::map(fd)
    }

    fn map(fd: OwnedFd) -> std::io::Result<Self> {
        // SAFETY: a shared mapping of a page of a memfd we hold; checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd, ptr })
    }

    /// The page's descriptor, to send.
    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    fn slot(&self, i: usize) -> &Slot {
        // SAFETY: `i % SLOTS` slots fit in the page (checked at compile time), the page is
        // mapped while `self` lives, and `Slot` is only atomics (any bit pattern is valid).
        unsafe { &*(self.ptr as *const Slot).add(i % SLOTS) }
    }

    /// Starts frame `id` in slot `i` with `rows` final (the service side).
    pub(super) fn begin(&self, i: usize, id: u64, rows: [u32; 2]) {
        let s = self.slot(i);
        s.state.store(RUNNING, Ordering::Relaxed);
        s.rows[0].store(rows[0], Ordering::Relaxed);
        s.rows[1].store(rows[1], Ordering::Relaxed);
        s.id.store(id, Ordering::Release);
        self.changed(s);
    }

    /// More rows of slot `i`'s frame are final, and its state (the service side).
    pub(super) fn update(&self, i: usize, rows: [u32; 2], state: ProgressState) {
        let s = self.slot(i);
        s.rows[0].fetch_max(rows[0], Ordering::Release);
        s.rows[1].fetch_max(rows[1], Ordering::Release);
        let state = match state {
            ProgressState::Running => RUNNING,
            ProgressState::Complete => COMPLETE,
            ProgressState::Failed => FAILED,
        };
        s.state.store(state, Ordering::Release);
        self.changed(s);
    }

    fn changed(&self, s: &Slot) {
        s.seq.fetch_add(1, Ordering::Release);
        // SAFETY: wakes waiters on a word of a shared mapping (a process-shared futex).
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                s.seq.as_ptr(),
                libc::FUTEX_WAKE,
                i32::MAX,
                std::ptr::null::<libc::timespec>(),
            );
        }
    }
}

impl Drop for ProgressRing {
    fn drop(&mut self) {
        // SAFETY: unmaps the page mapped in `map`, once.
        unsafe { libc::munmap(self.ptr, PAGE) };
    }
}

/// A received frame's progress, read from its slot (the client side). On first seeing the
/// frame complete it runs `on_complete` (the receiver's cache re-sync: rows below the first
/// slice may have been cached before the service's back end wrote them).
pub(super) struct SlotSource {
    ring: Arc<ProgressRing>,
    slot: usize,
    id: u64,
    /// Each output's height: every row once the frame is complete.
    heights: [u32; 2],
    on_complete: Box<dyn Fn() + Send + Sync>,
    completed: AtomicBool,
}

impl SlotSource {
    pub(super) fn new(
        ring: Arc<ProgressRing>,
        slot: usize,
        id: u64,
        heights: [u32; 2],
        on_complete: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            ring,
            slot,
            id,
            heights,
            on_complete,
            completed: AtomicBool::new(false),
        }
    }
}

impl ProgressSource for SlotSource {
    fn read(&self) -> ([u32; 2], ProgressState) {
        let s = self.ring.slot(self.slot);
        let state = s.state.load(Ordering::Acquire);
        let rows = [
            s.rows[0].load(Ordering::Acquire),
            s.rows[1].load(Ordering::Acquire),
        ];
        // The slot moved on to a later frame: this one completed before it did.
        let mine = s.id.load(Ordering::Acquire) == self.id;
        let state = match (mine, state) {
            (false, _) | (true, COMPLETE) => ProgressState::Complete,
            (true, FAILED) => return (rows, ProgressState::Failed),
            _ => return (rows, ProgressState::Running),
        };
        if !self.completed.swap(true, Ordering::AcqRel) {
            (self.on_complete)();
        }
        (self.heights, state)
    }

    fn wait(&self, timeout: Duration) {
        let s = self.ring.slot(self.slot);
        let seq = s.seq.load(Ordering::Acquire);
        if self.read().1 != ProgressState::Running {
            return;
        }
        // Fields set by inference: `time_t` differs between libcs (and is deprecated on musl).
        // SAFETY: a plain C struct; all zeroes is a valid value.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        ts.tv_sec = timeout.as_secs() as _;
        ts.tv_nsec = timeout.subsec_nanos() as _;
        // SAFETY: waits on a word of a shared mapping while it still holds `seq` (a
        // process-shared futex), at most `timeout`.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                s.seq.as_ptr(),
                libc::FUTEX_WAIT,
                seq,
                &ts as *const libc::timespec,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use styx_core::buffer::{FrameProgress, ProgressWait};

    use super::*;

    #[test]
    fn a_client_reads_and_waits_on_what_the_service_writes() {
        let service = ProgressRing::create().unwrap();
        let fd = service.fd().try_clone_to_owned().unwrap();
        let client = Arc::new(ProgressRing::from_fd(fd).unwrap());
        service.begin(3, 77, [552, 274]);
        let synced = Arc::new(AtomicU32::new(0));
        let s2 = synced.clone();
        let source = SlotSource::new(
            client.clone(),
            3,
            77,
            [800, 400],
            Box::new(move || {
                s2.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let p = Arc::new(FrameProgress::with_source(Arc::new(source)));
        assert_eq!(p.output(0).rows(), 552);
        assert_eq!(p.output(0).ready(600), None);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            service.update(3, [800, 400], ProgressState::Complete);
            service
        });
        let start = Instant::now();
        assert_eq!(
            p.output(1).wait_complete(Duration::from_secs(5)),
            ProgressWait::Complete
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "woken, not timed out"
        );
        assert_eq!((p.output(0).rows(), p.output(1).rows()), (800, 400));
        assert_eq!(synced.load(Ordering::Relaxed), 1, "re-synced once");
        let service = t.join().unwrap();
        // The slot reused for a later frame: this one reads as complete.
        service.begin(3 + SLOTS, 93, [10, 5]);
        let old = SlotSource::new(client, 3, 77, [800, 400], Box::new(|| {}));
        assert_eq!(old.read(), ([800, 400], ProgressState::Complete));
    }
}

//! The reactor thread: one epoll instance, one timer, wakers for everything waiting on them.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::task::Waker;
use std::time::Instant;

use super::sys::{self, Epoll, EventFd, TimerFd};
use super::{Interest, Ready};

const SHUTDOWN_TOKEN: u64 = 0;
const TIMER_TOKEN: u64 = 1;
const FIRST_TOKEN: u64 = 2;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here panics while holding a lock; recover rather than cascade if it ever does.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drives readiness and timers for futures on any executor (or none).
///
/// The reactor owns a thread that waits in `epoll_wait` and wakes the [`Waker`]s of futures
/// waiting on file descriptors or deadlines. Nothing is polled on that thread; it only wakes.
/// Most code uses [`Reactor::global`], which starts on first use and lives for the process. The
/// thread of a reactor made with [`Reactor::new`] stops when the last handle to it is dropped.
#[derive(Clone)]
pub struct Reactor {
    handle: Arc<Handle>,
}

/// Stops the thread when the last [`Reactor`] clone goes.
struct Handle {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    epoll: Epoll,
    shutdown_fd: EventFd,
    timer_fd: TimerFd,
    shutdown: AtomicBool,
    next_token: AtomicU64,
    registrations: Mutex<HashMap<u64, Weak<Registration>>>,
    timers: Mutex<Timers>,
}

#[derive(Default)]
struct Timers {
    entries: BTreeMap<(Instant, u64), Waker>,
    armed: Option<Instant>,
}

impl Reactor {
    /// Start a reactor with its own thread.
    pub fn new() -> io::Result<Reactor> {
        let inner = Arc::new(Inner {
            epoll: Epoll::new()?,
            shutdown_fd: EventFd::new()?,
            timer_fd: TimerFd::new()?,
            shutdown: AtomicBool::new(false),
            next_token: AtomicU64::new(FIRST_TOKEN),
            registrations: Mutex::new(HashMap::new()),
            timers: Mutex::new(Timers::default()),
        });
        inner
            .epoll
            .add(inner.shutdown_fd.fd(), sys::EPOLLIN, SHUTDOWN_TOKEN)?;
        inner
            .epoll
            .add(inner.timer_fd.fd(), sys::EPOLLIN, TIMER_TOKEN)?;
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("styx-reactor".into())
            .spawn(move || thread_inner.run())?;
        Ok(Reactor {
            handle: Arc::new(Handle { inner }),
        })
    }

    /// The process-wide reactor, started on first use.
    pub fn try_global() -> io::Result<&'static Reactor> {
        static GLOBAL: OnceLock<Result<Reactor, (io::ErrorKind, String)>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Reactor::new().map_err(|e| (e.kind(), e.to_string())))
            .as_ref()
            .map_err(|(kind, msg)| io::Error::new(*kind, format!("styx reactor: {msg}")))
    }

    /// The process-wide reactor.
    ///
    /// # Panics
    /// If the reactor cannot start (no epoll, eventfd or timerfd, or no thread).
    pub fn global() -> &'static Reactor {
        Self::try_global().expect("start the styx reactor")
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.handle.inner
    }

    /// Register `fd` (not yet armed for anything).
    pub(crate) fn register(&self, fd: RawFd) -> io::Result<Arc<Registration>> {
        let inner = self.inner();
        let token = inner.next_token.fetch_add(1, Ordering::Relaxed);
        let reg = Arc::new(Registration {
            fd,
            token,
            reactor: self.clone(),
            state: Mutex::new(RegState::default()),
        });
        lock(&inner.registrations).insert(token, Arc::downgrade(&reg));
        if let Err(err) = inner.epoll.add_oneshot(fd, 0, token) {
            lock(&inner.registrations).remove(&token);
            return Err(err);
        }
        Ok(reg)
    }

    /// Wake `waker` at `deadline`; returns a key for [`Reactor::update_timer`] and
    /// [`Reactor::cancel_timer`].
    pub(crate) fn add_timer(&self, deadline: Instant, waker: Waker) -> u64 {
        let inner = self.inner();
        let key = inner.next_token.fetch_add(1, Ordering::Relaxed);
        let mut timers = lock(&inner.timers);
        timers.entries.insert((deadline, key), waker);
        if timers.armed.is_none_or(|armed| deadline < armed) {
            timers.armed = Some(deadline);
            inner.arm_timer(deadline);
        }
        key
    }

    /// Replace the waker of a pending timer; false if it already fired.
    pub(crate) fn update_timer(&self, deadline: Instant, key: u64, waker: &Waker) -> bool {
        let mut timers = lock(&self.inner().timers);
        match timers.entries.get_mut(&(deadline, key)) {
            Some(existing) => {
                if !existing.will_wake(waker) {
                    *existing = waker.clone();
                }
                true
            }
            None => false,
        }
    }

    pub(crate) fn cancel_timer(&self, deadline: Instant, key: u64) {
        lock(&self.inner().timers).entries.remove(&(deadline, key));
    }
}

impl std::fmt::Debug for Reactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reactor").finish_non_exhaustive()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.shutdown_fd.notify();
    }
}

impl Inner {
    fn arm_timer(&self, deadline: Instant) {
        let after = deadline.saturating_duration_since(Instant::now());
        // Failing to arm a monotonic timerfd with a valid value does not happen in practice;
        // if it did, timers would fire on the next unrelated event instead.
        let _ = self.timer_fd.set(Some(after));
    }

    fn run(&self) {
        let mut events = Vec::with_capacity(64);
        let mut wake = Vec::new();
        loop {
            if self.epoll.wait(&mut events).is_err() {
                // EBADF/EINVAL cannot recover; leave waiters to their own timeouts.
                return;
            }
            for event in &events {
                match event.token {
                    SHUTDOWN_TOKEN => {
                        self.shutdown_fd.drain();
                        if self.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                    }
                    TIMER_TOKEN => self.fire_timers(&mut wake),
                    token => {
                        let reg = lock(&self.registrations)
                            .get(&token)
                            .and_then(Weak::upgrade);
                        if let Some(reg) = reg {
                            reg.dispatch(Ready::from_epoll(event.events), &mut wake);
                        }
                    }
                }
            }
            for waker in wake.drain(..) {
                waker.wake();
            }
        }
    }

    fn fire_timers(&self, wake: &mut Vec<Waker>) {
        self.timer_fd.drain();
        let now = Instant::now();
        let mut timers = lock(&self.timers);
        let later = timers.entries.split_off(&(now, u64::MAX));
        let expired = std::mem::replace(&mut timers.entries, later);
        wake.extend(expired.into_values());
        timers.armed = timers.entries.keys().next().map(|(deadline, _)| *deadline);
        if let Some(next) = timers.armed {
            self.arm_timer(next);
        }
    }
}

/// A file descriptor registered with a reactor, and the futures waiting on it.
///
/// Registrations are one-shot in epoll: armed for the union of what waiters want, disarmed
/// after each event, re-armed while anyone still waits. Waiting therefore always observes the
/// descriptor's current state (level-triggered), with no readiness cached between waits.
pub(crate) struct Registration {
    fd: RawFd,
    token: u64,
    reactor: Reactor,
    state: Mutex<RegState>,
}

#[derive(Default)]
struct RegState {
    waiters: Vec<Waiter>,
    next_id: u64,
    closed: bool,
}

struct Waiter {
    id: u64,
    interest: Interest,
    waker: Option<Waker>,
    fired: Ready,
}

impl RegState {
    fn wanted(&self) -> u32 {
        self.waiters
            .iter()
            .filter(|w| w.fired.is_empty())
            .fold(0, |bits, w| bits | w.interest.epoll_bits())
    }
}

impl Registration {
    fn rearm(&self, state: &RegState) -> io::Result<()> {
        let wanted = state.wanted();
        if wanted == 0 || state.closed {
            return Ok(());
        }
        self.reactor
            .inner()
            .epoll
            .rearm(self.fd, wanted, self.token)
    }

    fn dispatch(&self, ready: Ready, wake: &mut Vec<Waker>) {
        let mut state = lock(&self.state);
        for waiter in state.waiters.iter_mut().filter(|w| w.fired.is_empty()) {
            let hit = ready.matching(waiter.interest);
            if !hit.is_empty() {
                waiter.fired = hit;
                wake.extend(waiter.waker.take());
            }
        }
        // Anyone left is still waiting; a failure here surfaces on their next poll.
        let _ = self.rearm(&state);
    }

    /// Poll waiter `id` (0 = not yet added) for `interest`. Returns the new id while pending.
    pub(crate) fn poll_ready(
        &self,
        id: &mut u64,
        interest: Interest,
        waker: &Waker,
    ) -> Option<io::Result<Ready>> {
        let mut state = lock(&self.state);
        if state.closed {
            return Some(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "descriptor deregistered from the reactor",
            )));
        }
        if let Some(pos) = state.waiters.iter().position(|w| w.id == *id) {
            let waiter = &mut state.waiters[pos];
            if !waiter.fired.is_empty() {
                let fired = waiter.fired;
                state.waiters.swap_remove(pos);
                *id = 0;
                return Some(Ok(fired));
            }
            if !waiter.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
                waiter.waker = Some(waker.clone());
            }
            return None;
        }
        state.next_id += 1;
        *id = state.next_id;
        state.waiters.push(Waiter {
            id: *id,
            interest,
            waker: Some(waker.clone()),
            fired: Ready::EMPTY,
        });
        if let Err(err) = self.rearm(&state) {
            state.waiters.pop();
            *id = 0;
            return Some(Err(err));
        }
        None
    }

    /// Forget waiter `id` (a future dropped before completing).
    pub(crate) fn cancel(&self, id: u64) {
        if id != 0 {
            lock(&self.state).waiters.retain(|w| w.id != id);
        }
    }

    /// Remove the descriptor from epoll. Must run before the descriptor is closed.
    pub(crate) fn deregister(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.closed = true;
        let inner = self.reactor.inner();
        let _ = inner.epoll.delete(self.fd);
        lock(&inner.registrations).remove(&self.token);
        for waiter in &mut state.waiters {
            waiter.fired = Ready::ERROR;
            if let Some(w) = waiter.waker.take() {
                w.wake();
            }
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.deregister();
    }
}

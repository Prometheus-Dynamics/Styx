//! A minimal executor for blocking wrappers: run one future on the current thread.

use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

struct Unparker {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for Unparker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.woken.swap(true, Ordering::Release) {
            self.thread.unpark();
        }
    }
}

thread_local! {
    /// This thread's unparker and its waker, made on its first [`block_on`] and kept: a
    /// blocking loop (`next_blocking` once a frame) allocates nothing for them.
    static UNPARKER: (Arc<Unparker>, Waker) = new_unparker();
    /// Whether a [`block_on`] on this thread is using [`UNPARKER`].
    static IN_USE: Cell<bool> = const { Cell::new(false) };
}

/// Run `future` to completion on this thread, parking while it is pending.
///
/// Meant for blocking wrappers (`frames.next_blocking()`); inside an async runtime, `.await`
/// instead.
pub fn block_on<F: Future>(future: F) -> F::Output {
    // A `block_on` inside a future polled by another (rare) gets its own unparker, so the
    // outer one's wake-up is not taken.
    if IN_USE.with(|u| u.replace(true)) {
        return run(future, &new_unparker());
    }
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            IN_USE.with(|u| u.set(false));
        }
    }
    let _release = Release;
    // After the thread's locals are gone (a `block_on` in a destructor), a new one.
    let mut future = Some(future);
    UNPARKER
        .try_with(|pair| run(future.take().expect("not run yet"), pair))
        .unwrap_or_else(|_| run(future.take().expect("not run yet"), &new_unparker()))
}

fn new_unparker() -> (Arc<Unparker>, Waker) {
    let unparker = Arc::new(Unparker {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(unparker.clone());
    (unparker, waker)
}

fn run<F: Future>(future: F, (unparker, waker): &(Arc<Unparker>, Waker)) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        while !unparker.woken.swap(false, Ordering::Acquire) {
            thread::park();
        }
    }
}

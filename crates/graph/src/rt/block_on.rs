//! A minimal executor for blocking wrappers: run one future on the current thread.

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

/// Run `future` to completion on this thread, parking while it is pending.
///
/// Meant for blocking wrappers (`frames.next_blocking()`); inside an async runtime, `.await`
/// instead.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let unparker = Arc::new(Unparker {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(unparker.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        while !unparker.woken.swap(false, Ordering::Acquire) {
            thread::park();
        }
    }
}

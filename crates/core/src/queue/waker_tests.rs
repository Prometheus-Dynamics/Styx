//! The waker-based receive and send (no `std` needed: what a firmware's executor or superloop
//! uses).

use super::{RecvOutcome, SendOutcome, bounded};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// A waker that counts its wakes.
struct CountingWaker(AtomicUsize);

impl std::task::Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn poll_recv_registers_the_waker_and_a_send_wakes_it() {
    use std::task::{Context, Poll, Waker};
    let (tx, rx) = bounded::<u32>(2);
    let count = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(rx.poll_recv(&mut cx).is_pending());
    // Polling again with the same waker registers it once (`will_wake`; best effort, so
    // the counts below allow a duplicate).
    assert!(rx.poll_recv(&mut cx).is_pending());
    assert_eq!(tx.send(5), SendOutcome::Ok);
    let woken = count.0.load(Ordering::SeqCst);
    assert!((1..=2).contains(&woken), "{woken}");
    assert!(matches!(
        rx.poll_recv(&mut cx),
        Poll::Ready(RecvOutcome::Data(5))
    ));
    // A send with nobody waiting wakes nothing; closing wakes a waiting receiver.
    assert_eq!(tx.send(6), SendOutcome::Ok);
    assert!(matches!(
        rx.poll_recv(&mut cx),
        Poll::Ready(RecvOutcome::Data(6))
    ));
    assert!(rx.poll_recv(&mut cx).is_pending());
    tx.close();
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        woken + 1,
        "closing wakes the receiver"
    );
    assert!(matches!(
        rx.poll_recv(&mut cx),
        Poll::Ready(RecvOutcome::Closed)
    ));
    assert_eq!(rx.stats().async_recv_waits, 3, "one per pending poll");
}

#[test]
fn send_async_waits_for_room_on_a_superloop() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let (tx, rx) = bounded::<u32>(1);
    assert_eq!(tx.send(1), SendOutcome::Ok);
    let count = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let mut cx = Context::from_waker(&waker);
    let mut send = std::pin::pin!(tx.send_async(2));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(rx.recv(), RecvOutcome::Data(1)));
    assert_eq!(count.0.load(Ordering::SeqCst), 1, "room wakes the sender");
    assert_eq!(send.as_mut().poll(&mut cx), Poll::Ready(SendOutcome::Ok));
    assert!(matches!(rx.recv(), RecvOutcome::Data(2)));
}

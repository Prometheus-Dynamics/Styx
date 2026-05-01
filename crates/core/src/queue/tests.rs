use super::{RecvOutcome, RecvWaitOutcome, SendOutcome, SendWaitOutcome, bounded};
use std::thread;
use std::time::Duration;

#[test]
fn send_timeout_returns_value_when_queue_stays_full() {
    let (tx, rx) = bounded(1);
    assert!(matches!(tx.send(1), SendOutcome::Ok));

    match tx.send_timeout(2, Duration::from_millis(5)) {
        SendWaitOutcome::Timeout(value) => assert_eq!(value, 2),
        other => panic!("expected timeout, got {other:?}"),
    }

    match rx.recv() {
        RecvOutcome::Data(value) => assert_eq!(value, 1),
        other => panic!("expected queued value, got {other:?}"),
    }
}

#[test]
fn send_timeout_wakes_when_receiver_makes_capacity() {
    let (tx, rx) = bounded(1);
    assert!(matches!(tx.send(1), SendOutcome::Ok));

    let rx_worker = rx.clone();
    let join = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        match rx_worker.recv_blocking() {
            RecvWaitOutcome::Data(value) => assert_eq!(value, 1),
            other => panic!("expected first queued value, got {other:?}"),
        }
    });

    assert!(matches!(
        tx.send_timeout(2, Duration::from_secs(1)),
        SendWaitOutcome::Ok
    ));

    join.join().expect("receiver thread");
    match rx.recv() {
        RecvOutcome::Data(value) => assert_eq!(value, 2),
        other => panic!("expected second queued value, got {other:?}"),
    }
}

#[test]
fn send_timeout_does_not_miss_repeated_capacity_wakes() {
    for _ in 0..200 {
        let (tx, rx) = bounded(1);
        assert!(matches!(tx.send(1), SendOutcome::Ok));

        let rx_worker = rx.clone();
        let join = thread::spawn(move || {
            thread::sleep(Duration::from_micros(100));
            assert!(matches!(
                rx_worker.recv_blocking(),
                RecvWaitOutcome::Data(1)
            ));
        });

        assert!(matches!(
            tx.send_timeout(2, Duration::from_millis(250)),
            SendWaitOutcome::Ok
        ));
        join.join().expect("receiver thread");
    }
}

#[test]
fn recv_timeout_does_not_miss_repeated_data_wakes() {
    for _ in 0..200 {
        let (tx, rx) = bounded(1);

        let tx_worker = tx.clone();
        let join = thread::spawn(move || {
            thread::sleep(Duration::from_micros(100));
            assert!(matches!(tx_worker.send_blocking(1), SendWaitOutcome::Ok));
        });

        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(250)),
            RecvWaitOutcome::Data(1)
        ));
        join.join().expect("sender thread");
    }
}

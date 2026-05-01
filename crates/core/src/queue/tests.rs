use super::{RecvOutcome, RecvWaitOutcome, SendOutcome, SendWaitOutcome, bounded};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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

#[test]
fn mpmc_contention_drains_all_messages_before_close() {
    const PRODUCERS: usize = 4;
    const CONSUMERS: usize = 4;
    const PER_PRODUCER: usize = 512;
    const TOTAL: usize = PRODUCERS * PER_PRODUCER;

    let (tx, rx) = bounded(32);
    let received = Arc::new(AtomicUsize::new(0));
    let mut consumers = Vec::new();
    for _ in 0..CONSUMERS {
        let rx = rx.clone();
        let received = Arc::clone(&received);
        consumers.push(thread::spawn(move || {
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    RecvWaitOutcome::Data(_) => {
                        received.fetch_add(1, Ordering::Relaxed);
                    }
                    RecvWaitOutcome::Timeout => {}
                    RecvWaitOutcome::Closed => break,
                }
            }
        }));
    }

    let mut producers = Vec::new();
    for producer in 0..PRODUCERS {
        let tx = tx.clone();
        producers.push(thread::spawn(move || {
            for value in 0..PER_PRODUCER {
                let item = producer * PER_PRODUCER + value;
                assert!(matches!(tx.send_blocking(item), SendWaitOutcome::Ok));
            }
        }));
    }

    for producer in producers {
        producer.join().expect("producer thread");
    }
    tx.close();
    for consumer in consumers {
        consumer.join().expect("consumer thread");
    }

    assert_eq!(received.load(Ordering::Relaxed), TOTAL);
}

#[test]
fn close_wakes_blocked_sender_and_receiver() {
    let (tx, rx) = bounded(1);
    assert_eq!(tx.send(1), SendOutcome::Ok);

    let sender = {
        let tx = tx.clone();
        thread::spawn(move || tx.send_blocking(2))
    };
    thread::sleep(Duration::from_millis(10));
    rx.close();

    assert!(matches!(
        sender.join().expect("sender thread"),
        SendWaitOutcome::Closed(2)
    ));
    assert!(matches!(rx.recv_blocking(), RecvWaitOutcome::Data(1)));
    assert!(matches!(rx.recv_blocking(), RecvWaitOutcome::Closed));

    let (_tx, rx) = bounded::<u8>(1);
    let receiver = {
        let rx = rx.clone();
        thread::spawn(move || rx.recv_blocking())
    };
    thread::sleep(Duration::from_millis(10));
    rx.close();

    assert!(matches!(
        receiver.join().expect("receiver thread"),
        RecvWaitOutcome::Closed
    ));
}

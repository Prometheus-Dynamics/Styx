//! Frame streams and controls under tokio (multi-thread and current-thread), under the
//! runtime's own `block_on`, and with cancellation at every await point: a cancelled wait
//! loses no frame and leaks nothing.

use std::time::Duration;

use styx_graph::rt;

use crate::ControlHandle;
use crate::fault_tests::{Rig, WAIT};
use crate::stream::FrameStream;

/// Receives `n` frames, cancelling the wait whenever it takes longer than `patience`.
/// Returns the sequences received and how many waits were cancelled.
async fn receive_with_cancellation(
    stream: &mut FrameStream,
    n: usize,
    patience: Duration,
) -> (Vec<u32>, usize) {
    let mut seqs = Vec::new();
    let mut cancelled = 0;
    while seqs.len() < n {
        match tokio::time::timeout(patience, rt::next(stream)).await {
            Ok(Some(Ok(f))) => seqs.push(f.sequence),
            Ok(other) => panic!("stream ended: {other:?}"),
            Err(_) => cancelled += 1,
        }
    }
    (seqs, cancelled)
}

fn check(stream: &FrameStream, seqs: &[u32]) {
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    // Every frame taken from the queue was handed out: none lost to a cancelled wait.
    assert_eq!(stream.stats().frames, seqs.len() as u64);
    assert_eq!(stream.outstanding(), 0);
}

#[test]
fn frames_under_a_multi_thread_tokio_runtime_with_cancellation() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let controls = ControlHandle::new(rig.control.clone(), None);
    let _producer = rig.queue.run(Duration::from_millis(2));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    let (seqs, cancelled) = runtime.block_on(async {
        // Controls from another task meanwhile.
        let ctl = tokio::spawn(async move {
            for i in 0..50u64 {
                controls
                    .set_exposure(Duration::from_micros(2000 + i * 10))
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let r = receive_with_cancellation(&mut stream, 200, Duration::from_micros(700)).await;
        ctl.await.unwrap();
        r
    });
    assert!(cancelled > 0, "the test cancels waits");
    check(&stream, &seqs);
    drop(_producer);
    rig.session.stop().unwrap();
    // The stream ends after the stop, under the runtime too.
    assert!(runtime.block_on(rt::next(&mut stream)).is_none());
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn frames_under_a_current_thread_tokio_runtime_with_select() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let _producer = rig.queue.run(Duration::from_millis(1));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let seqs = runtime.block_on(async {
        let mut seqs = Vec::new();
        let mut ticks = tokio::time::interval(Duration::from_micros(500));
        while seqs.len() < 100 {
            tokio::select! {
                f = rt::next(&mut stream) => seqs.push(f.unwrap().unwrap().sequence),
                _ = ticks.tick() => {}
            }
        }
        seqs
    });
    check(&stream, &seqs);
    drop(_producer);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn a_task_waiting_for_frames_can_be_aborted_and_the_camera_stopped() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .unwrap();
    // No frames come: the task waits until aborted, then the stream is dropped with it.
    let task = runtime.spawn(async move { rt::next(&mut stream).await.map(|r| r.is_ok()) });
    std::thread::sleep(Duration::from_millis(30));
    task.abort();
    assert!(runtime.block_on(task).unwrap_err().is_cancelled());
    rig.session.stop().unwrap();
    rig.assert_stopped();
    // And again, this time stopping the camera while the task waits: the stream ends.
    let mut stream = rig.start().unwrap();
    let task = runtime.spawn(async move { rt::next(&mut stream).await.is_none() });
    std::thread::sleep(Duration::from_millis(30));
    rig.session.stop().unwrap();
    assert!(runtime.block_on(task).unwrap(), "the stream ended");
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn frames_without_any_runtime() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let _producer = rig.queue.run(Duration::from_millis(1));
    // The blocking wrapper, and the runtime-agnostic block_on.
    let a = stream.next_blocking(WAIT).unwrap().unwrap().sequence;
    let b = rt::block_on(rt::next(&mut stream))
        .unwrap()
        .unwrap()
        .sequence;
    assert!(b > a);
    // Timeouts that expire cancel nothing either.
    let mut seqs = vec![a, b];
    while seqs.len() < 50 {
        match stream.next_blocking(Duration::from_micros(300)) {
            Ok(Some(f)) => seqs.push(f.sequence),
            Ok(None) => panic!("ended"),
            Err(e) => assert!(matches!(e, crate::NativeError::Timeout), "{e}"),
        }
    }
    check(&stream, &seqs);
    drop(_producer);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

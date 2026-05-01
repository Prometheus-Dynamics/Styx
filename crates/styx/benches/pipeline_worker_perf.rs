use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;
use styx::prelude::*;

fn bench_raw_pipeline_drain(c: &mut Criterion) {
    c.bench_function("pipeline_raw_virtual_try_next_3_frames", |b| {
        b.iter(|| {
            let device = CaptureRequest::virtual_source(
                VirtualSourceConfig::new()
                    .name("bench-virtual")
                    .resolution(640, 360)
                    .fps(30),
            )
            .into_device();
            let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
                .config(StyxConfig::new().capture_queue_depth(3))
                .raw_frames()
                .start()
                .expect("start raw pipeline");

            let mut frames = 0usize;
            while frames < 3 {
                match pipeline.next_blocking(Duration::from_millis(100)) {
                    RecvOutcome::Data(frame) => {
                        frames += 1;
                        black_box(frame.payload_bytes());
                    }
                    RecvOutcome::Empty => break,
                    RecvOutcome::Closed => break,
                }
            }
            black_box(frames);
        });
    });
}

fn bench_capture_queue_backpressure(c: &mut Criterion) {
    c.bench_function("capture_queue_send_timeout_full_queue", |b| {
        let (tx, _rx) = styx_core::queue::bounded::<usize>(1);
        assert_eq!(tx.send(1), SendOutcome::Ok);
        b.iter(|| match tx.send_timeout(black_box(2), Duration::ZERO) {
            styx_core::queue::SendWaitOutcome::Timeout(value) => black_box(value),
            other => panic!("expected timeout, got {other:?}"),
        });
    });
}

fn bench_queue_contention(c: &mut Criterion) {
    c.bench_function("core_queue_mpmc_contention_4x4_4096_msgs", |b| {
        b.iter(|| {
            const PRODUCERS: usize = 4;
            const CONSUMERS: usize = 4;
            const MESSAGES_PER_PRODUCER: usize = 1024;
            const TOTAL: usize = PRODUCERS * MESSAGES_PER_PRODUCER;

            let (tx, rx) = styx_core::queue::bounded::<usize>(64);
            let remaining = Arc::new(AtomicUsize::new(TOTAL));
            let mut consumers = Vec::with_capacity(CONSUMERS);
            for _ in 0..CONSUMERS {
                let rx = rx.clone();
                let remaining = Arc::clone(&remaining);
                consumers.push(thread::spawn(move || {
                    let mut received = 0usize;
                    loop {
                        if remaining.load(Ordering::Acquire) == 0 {
                            break;
                        }
                        match rx.recv_blocking() {
                            styx_core::queue::RecvWaitOutcome::Data(value) => {
                                black_box(value);
                                received += 1;
                                remaining.fetch_sub(1, Ordering::AcqRel);
                            }
                            styx_core::queue::RecvWaitOutcome::Closed => break,
                            styx_core::queue::RecvWaitOutcome::Timeout => {}
                        }
                    }
                    received
                }));
            }

            let mut producers = Vec::with_capacity(PRODUCERS);
            for producer in 0..PRODUCERS {
                let tx = tx.clone();
                producers.push(thread::spawn(move || {
                    for value in 0..MESSAGES_PER_PRODUCER {
                        let mut payload = producer * MESSAGES_PER_PRODUCER + value;
                        loop {
                            match tx.send_blocking(payload) {
                                styx_core::queue::SendWaitOutcome::Ok => break,
                                styx_core::queue::SendWaitOutcome::Closed(value) => {
                                    payload = value;
                                }
                                styx_core::queue::SendWaitOutcome::Timeout(value) => {
                                    payload = value;
                                }
                            }
                        }
                    }
                }));
            }

            for producer in producers {
                producer.join().expect("producer");
            }
            tx.close();
            let received: usize = consumers
                .into_iter()
                .map(|consumer| consumer.join().expect("consumer"))
                .sum();
            black_box(received);
            assert_eq!(received, TOTAL);
        });
    });
}

#[cfg(feature = "async")]
fn bench_async_receive(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");

    c.bench_function("async_queue_recv_ready_frame", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let (tx, rx) = styx_core::queue::bounded::<usize>(1);
                assert_eq!(tx.send(black_box(7)), SendOutcome::Ok);
                match rx.recv_async().await {
                    RecvOutcome::Data(value) => black_box(value),
                    other => panic!("expected ready frame, got {other:?}"),
                }
            })
        });
    });
}

fn benches(c: &mut Criterion) {
    bench_raw_pipeline_drain(c);
    bench_capture_queue_backpressure(c);
    bench_queue_contention(c);
    #[cfg(feature = "async")]
    bench_async_receive(c);
}

criterion_group!(pipeline_worker_benches, benches);
criterion_main!(pipeline_worker_benches);

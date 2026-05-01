use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
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
    #[cfg(feature = "async")]
    bench_async_receive(c);
}

criterion_group!(pipeline_worker_benches, benches);
criterion_main!(pipeline_worker_benches);

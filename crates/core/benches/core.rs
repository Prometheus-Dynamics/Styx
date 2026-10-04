//! The frame path's core pieces on the host: pooled buffers, frames over them, shared views,
//! the bounded queues, a packed transform and a pyramid level (1280x800).
//!
//! `cargo bench -p styx-core-rs`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use styx_core::buffer::ExternalBacking;
use styx_core::prelude::*;
use styx_core::transform::{FrameTransform, Rotation90, transform_packed_frame};

const W: u32 = 1280;
const H: u32 = 800;

fn grey_frame(pool: &BufferPool) -> FrameLease {
    let len = (W * H) as usize;
    let mut buf = pool.lease();
    buf.resize(len);
    let format = MediaFormat::new(
        FourCc::GREY,
        Resolution::new(W, H).unwrap(),
        ColorSpace::Unknown,
    );
    FrameLease::single_plane(FrameMeta::new(format, 1), buf, len, W as usize)
}

struct Static(Vec<u8>);

impl ExternalBacking for Static {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index == 0).then_some(self.0.as_slice())
    }
}

fn buffers(c: &mut Criterion) {
    let pool = BufferPool::with_capacity(4, (W * H) as usize);
    c.bench_function("pool/lease_drop", |b| {
        b.iter(|| black_box(pool.lease()));
    });
    c.bench_function("pool/lease_sized_drop", |b| {
        b.iter(|| black_box(pool.lease_sized(black_box(4096))));
    });
    c.bench_function("frame/single_plane_planes", |b| {
        b.iter(|| {
            let frame = grey_frame(&pool);
            black_box(frame.planes()[0].data().len())
        });
    });
    let backing: Arc<dyn ExternalBacking> = Arc::new(Static(vec![7; (W * H) as usize]));
    let format = MediaFormat::new(
        FourCc::GREY,
        Resolution::new(W, H).unwrap(),
        ColorSpace::Unknown,
    );
    c.bench_function("frame/from_external_planes", |b| {
        b.iter(|| {
            let frame = FrameLease::from_external(
                FrameMeta::new(format, 1),
                smallvec::smallvec![plane_layout_from_dims(
                    format.resolution.width,
                    format.resolution.height,
                    1
                )],
                backing.clone(),
            );
            black_box(frame.planes()[0].data().len())
        });
    });
    c.bench_function("frame/into_shareable_share2", |b| {
        b.iter(|| {
            let frame = grey_frame(&pool).into_shareable();
            let views = (frame.share().unwrap(), frame.share().unwrap());
            black_box(views.0.planes()[0].data().len() + views.1.planes()[0].data().len())
        });
    });
    c.bench_function("frame/box_pyramid_level", |b| {
        let levels = BufferPool::lazy(0, 2);
        b.iter(|| {
            let frame = grey_frame(&pool)
                .with_box_pyramid_in(1, 64, &levels)
                .unwrap();
            black_box(frame.pyramid_level(1).is_some())
        });
    });
    let frame = grey_frame(&pool);
    c.bench_function("transform/rotate90_grey", |b| {
        let t = FrameTransform {
            rotation: Rotation90::Deg90,
            mirror: false,
        };
        b.iter(|| black_box(transform_packed_frame(&frame, t).unwrap()));
    });
    c.bench_function("frame/crop_view", |b| {
        b.iter(|| {
            black_box(
                grey_frame(&pool)
                    .crop_view(FrameRect {
                        x: 64,
                        y: 64,
                        width: 640,
                        height: 400,
                    })
                    .unwrap(),
            )
        });
    });
}

fn queues(c: &mut Criterion) {
    let (tx, rx) = bounded::<u64>(8);
    c.bench_function("queue/send_recv", |b| {
        b.iter(|| {
            let _ = tx.send(black_box(1));
            black_box(rx.recv())
        });
    });
    let (tx, rx) = bounded_with::<u64>(2, QueueOverflow::DropOldest);
    c.bench_function("queue/drop_oldest_send3_recv", |b| {
        b.iter(|| {
            let _ = tx.send(1);
            let _ = tx.send(2);
            let _ = tx.send(3);
            black_box(rx.recv())
        });
    });
    let (tx, rx) = bounded::<u64>(8);
    c.bench_function("queue/send_recv_timeout", |b| {
        b.iter(|| {
            let _ = tx.send(black_box(1));
            black_box(rx.recv_timeout(Duration::from_millis(1)))
        });
    });
    let (tx, rx) = bounded::<u64>(64);
    c.bench_function("queue/threaded_1k", |b| {
        b.iter(|| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    for i in 0..1000 {
                        let _ = tx.send_blocking(i);
                    }
                });
                let mut n = 0;
                while n < 1000 {
                    if let RecvWaitOutcome::Data(_) = rx.recv_blocking() {
                        n += 1;
                    }
                }
            });
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = buffers, queues
}
criterion_main!(benches);

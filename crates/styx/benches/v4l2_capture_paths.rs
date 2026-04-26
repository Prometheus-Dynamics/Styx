use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use smallvec::smallvec;
use styx_core::prelude::*;

struct BenchBacking {
    data: Arc<[u8]>,
}

impl ExternalBacking for BenchBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        match index {
            0 => Some(&self.data),
            _ => None,
        }
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.data.len())
    }

    fn backing_kind(&self) -> &'static str {
        "bench_v4l2_mmap"
    }
}

fn bench_capture_path_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("v4l2_capture_frame_construction");
    let cases = [
        ("720p_yuyv", 1280u32, 720u32, 2usize),
        ("1080p_yuyv", 1920u32, 1080u32, 2usize),
        ("4k_yuyv", 3840u32, 2160u32, 2usize),
    ];

    for (name, width, height, bytes_per_pixel) in cases {
        let resolution = Resolution::new(width, height).expect("resolution");
        let format = MediaFormat::new(FourCc::new(*b"YUYV"), resolution, ColorSpace::Unknown);
        let layout = plane_layout_from_dims(resolution.width, resolution.height, bytes_per_pixel);
        let frame_bytes = layout.len;
        let source = vec![0x5a; frame_bytes];
        let pool = BufferPool::with_limits(4, frame_bytes, 8);
        let backing_data: Arc<[u8]> = source.clone().into();
        let backing: Arc<dyn ExternalBacking> = Arc::new(BenchBacking { data: backing_data });

        group.throughput(Throughput::Bytes(frame_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("copy_path", name),
            &frame_bytes,
            |b, &_len| {
                b.iter(|| {
                    let meta = FrameMeta::new(format, 0);
                    let mut lease = pool.lease();
                    lease.resize(frame_bytes);
                    lease.as_mut_slice()[..frame_bytes].copy_from_slice(&source[..frame_bytes]);
                    let frame = FrameLease::multi_plane(meta, smallvec![lease], smallvec![layout]);
                    black_box(frame.payload_bytes());
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("external_path", name),
            &frame_bytes,
            |b, &_len| {
                b.iter(|| {
                    let meta = FrameMeta::new(format, 0);
                    let frame =
                        FrameLease::from_external(meta, smallvec![layout], Arc::clone(&backing));
                    let planes = frame.planes();
                    black_box(planes[0].data().len());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_capture_path_construction);
criterion_main!(benches);

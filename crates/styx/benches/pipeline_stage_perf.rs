use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use image::codecs::jpeg::JpegEncoder as ImageJpegEncoder;
use image::{ColorType, RgbImage};
use std::hint::black_box;
use styx::prelude::*;

fn build_sample_jpeg(width: u32, height: u32, quality: u8) -> Vec<u8> {
    let mut img = RgbImage::new(width, height);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        *pixel = image::Rgb([
            ((x * 7 + y * 3) & 0xff) as u8,
            ((x * 5 + y * 11) & 0xff) as u8,
            ((x * 13 + y * 17) & 0xff) as u8,
        ]);
    }

    let mut encoded = Vec::new();
    let mut encoder = ImageJpegEncoder::new_with_quality(&mut encoded, quality);
    encoder
        .encode(img.as_raw(), width, height, ColorType::Rgb8.into())
        .expect("encode jpeg sample");
    encoded
}

fn build_mjpeg_frame(encoded: &[u8], width: u32, height: u32) -> FrameLease {
    let pool = BufferPool::with_limits(2, encoded.len(), 2);
    let mut buf = pool.lease();
    buf.resize(encoded.len());
    buf.as_mut_slice().copy_from_slice(encoded);
    let res = Resolution::new(width, height).expect("resolution");
    let format = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
    FrameLease::single_plane(FrameMeta::new(format, 0), buf, encoded.len(), encoded.len())
}

fn build_rg24_frame(width: u32, height: u32) -> FrameLease {
    let res = Resolution::new(width, height).expect("resolution");
    let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
    let layout = plane_layout_from_dims(res.width, res.height, 3);
    let pool = BufferPool::with_limits(2, layout.len, 2);
    let mut buf = pool.lease();
    buf.resize(layout.len);
    for (idx, byte) in buf.as_mut_slice().iter_mut().enumerate() {
        *byte = (idx % 251) as u8;
    }
    FrameLease::single_plane(FrameMeta::new(format, 0), buf, layout.len, layout.stride)
}

fn bench_mjpeg_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("mjpeg_decode");
    let cases = [("640x360", 640u32, 360u32), ("1280x720", 1280u32, 720u32)];

    for (name, width, height) in cases {
        let encoded = build_sample_jpeg(width, height, 85);
        let decoder = MjpegDecoder::new(FourCc::RG24);

        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("decode_to_rg24", name),
            &encoded,
            |b, data| {
                b.iter(|| {
                    let frame = build_mjpeg_frame(data, width, height);
                    let decoded = decoder.process(frame).expect("decode frame");
                    black_box(decoded.payload_bytes());
                });
            },
        );
    }

    group.finish();
}

fn bench_packed_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("packed_frame_transform");
    let cases = [
        ("rotate90_720p", Rotation90::Deg90, false),
        ("mirror_720p", Rotation90::Deg0, true),
    ];
    let frame = build_rg24_frame(1280, 720);
    let throughput = frame.payload_bytes() as u64;

    for (name, rotation, mirror) in cases {
        group.throughput(Throughput::Bytes(throughput));
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = transform_packed_frame(&frame, FrameTransform { rotation, mirror })
                    .expect("transform frame");
                black_box(out.payload_bytes());
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_mjpeg_decode, bench_packed_transform);
criterion_main!(benches);

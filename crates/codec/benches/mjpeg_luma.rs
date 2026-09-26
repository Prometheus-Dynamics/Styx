//! MJPEG decode paths on real Logitech C270 720p frames (4:2:2, restart markers).

use criterion::{Criterion, criterion_group, criterion_main};
use styx_codec::prelude::*;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testing/fixtures/c270_720p_rst.mjpeg"
);

fn fixture_frames() -> Vec<Vec<u8>> {
    let data = std::fs::read(FIXTURE).expect("read C270 fixture");
    let starts: Vec<usize> = (0..data.len() - 2)
        .filter(|&i| data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| data[s..*starts.get(k + 1).unwrap_or(&data.len())].to_vec())
        .collect()
}

fn mjpeg_frame(jpeg: &[u8]) -> FrameLease {
    let pool = BufferPool::with_limits(1, jpeg.len(), 1);
    let mut buf = pool.lease();
    buf.resize(jpeg.len());
    buf.as_mut_slice().copy_from_slice(jpeg);
    let res = Resolution::new(1280, 720).unwrap();
    FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 0),
        buf,
        jpeg.len(),
        jpeg.len(),
    )
}

fn bench_decoder(c: &mut Criterion, name: &str, codec: &dyn Codec, frames: &[Vec<u8>]) {
    let mut idx = 0;
    c.bench_function(name, |b| {
        b.iter(|| {
            let frame = mjpeg_frame(&frames[idx % frames.len()]);
            idx += 1;
            std::hint::black_box(codec.process(frame).expect("decode"));
        })
    });
}

fn mjpeg_luma(c: &mut Criterion) {
    let frames = fixture_frames();
    let luma = |options: LumaDecodeOptions| TurbojpegLumaDecoder::with_options(options);
    bench_decoder(
        c,
        "c270_720p/turbojpeg_rgb",
        &TurbojpegDecoder::new(FourCc::RG24),
        &frames,
    );
    bench_decoder(
        c,
        "c270_720p/luma_single_thread",
        &luma(LumaDecodeOptions {
            threads: 1,
            ..Default::default()
        }),
        &frames,
    );
    bench_decoder(
        c,
        "c270_720p/luma_auto_threads",
        &luma(LumaDecodeOptions::default()),
        &frames,
    );
    bench_decoder(
        c,
        "c270_720p/luma_auto_threads_half_pyramid",
        &luma(LumaDecodeOptions {
            pyramid_levels: 1,
            ..Default::default()
        }),
        &frames,
    );

    let decoded = luma(LumaDecodeOptions::default())
        .process(mjpeg_frame(&frames[0]))
        .expect("decode");
    let pool = BufferPool::lazy(0, 4);
    c.bench_function("c270_720p/box_downscale_half", |b| {
        b.iter(|| std::hint::black_box(box_downscale_luma_in(&decoded, 64, &pool).unwrap()))
    });
}

criterion_group!(benches, mjpeg_luma);
criterion_main!(benches);

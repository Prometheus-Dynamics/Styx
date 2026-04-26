use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder as ImageJpegEncoder;
use image::{ColorType, RgbImage};
use styx::prelude::*;

const DECODE_ITERATIONS: usize = 40;
const TRANSFORM_ITERATIONS: usize = 60;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let decoder = MjpegDecoder::new(FourCc::new(*b"RG24"));
    let jpeg = build_sample_jpeg(640, 360, 85);
    let decode_samples = measure_decode(&decoder, &jpeg, 640, 360, DECODE_ITERATIONS);

    let transform_frame = build_rg24_frame(1280, 720);
    let rotate_samples = measure_transform(
        &transform_frame,
        FrameTransform {
            rotation: Rotation90::Deg90,
            mirror: false,
        },
        TRANSFORM_ITERATIONS,
    );
    let mirror_samples = measure_transform(
        &transform_frame,
        FrameTransform {
            rotation: Rotation90::Deg0,
            mirror: true,
        },
        TRANSFORM_ITERATIONS,
    );

    let decode_p95 = percentile(&decode_samples, 0.95);
    let rotate_p95 = percentile(&rotate_samples, 0.95);
    let mirror_p95 = percentile(&mirror_samples, 0.95);

    println!(
        "decode_640x360 p50_ms={:.2} p95_ms={:.2}",
        percentile(&decode_samples, 0.50).as_secs_f64() * 1_000.0,
        decode_p95.as_secs_f64() * 1_000.0
    );
    println!(
        "rotate90_720p p50_ms={:.2} p95_ms={:.2}",
        percentile(&rotate_samples, 0.50).as_secs_f64() * 1_000.0,
        rotate_p95.as_secs_f64() * 1_000.0
    );
    println!(
        "mirror_720p p50_ms={:.2} p95_ms={:.2}",
        percentile(&mirror_samples, 0.50).as_secs_f64() * 1_000.0,
        mirror_p95.as_secs_f64() * 1_000.0
    );

    // These are intentionally loose so the command is usable as a smoke test across developer
    // machines and future CI runners. They are regression guards, not tight tuning targets.
    let decode_limit = Duration::from_millis(25);
    let transform_limit = Duration::from_millis(10);

    if decode_p95 > decode_limit {
        return Err(format!(
            "decode p95 {:.2}ms exceeded {:.2}ms",
            decode_p95.as_secs_f64() * 1_000.0,
            decode_limit.as_secs_f64() * 1_000.0
        )
        .into());
    }
    if rotate_p95 > transform_limit {
        return Err(format!(
            "rotate p95 {:.2}ms exceeded {:.2}ms",
            rotate_p95.as_secs_f64() * 1_000.0,
            transform_limit.as_secs_f64() * 1_000.0
        )
        .into());
    }
    if mirror_p95 > transform_limit {
        return Err(format!(
            "mirror p95 {:.2}ms exceeded {:.2}ms",
            mirror_p95.as_secs_f64() * 1_000.0,
            transform_limit.as_secs_f64() * 1_000.0
        )
        .into());
    }

    Ok(())
}

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
    let format = MediaFormat::new(FourCc::new(*b"MJPG"), res, ColorSpace::Srgb);
    FrameLease::single_plane(FrameMeta::new(format, 0), buf, encoded.len(), encoded.len())
}

fn build_rg24_frame(width: u32, height: u32) -> FrameLease {
    let res = Resolution::new(width, height).expect("resolution");
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let layout = plane_layout_from_dims(res.width, res.height, 3);
    let pool = BufferPool::with_limits(2, layout.len, 2);
    let mut buf = pool.lease();
    buf.resize(layout.len);
    for (idx, byte) in buf.as_mut_slice().iter_mut().enumerate() {
        *byte = (idx % 251) as u8;
    }
    FrameLease::single_plane(FrameMeta::new(format, 0), buf, layout.len, layout.stride)
}

fn measure_decode(
    decoder: &MjpegDecoder,
    jpeg: &[u8],
    width: u32,
    height: u32,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let frame = build_mjpeg_frame(jpeg, width, height);
        let start = Instant::now();
        let decoded = decoder.process(frame).expect("decode frame");
        samples.push(start.elapsed());
        std::hint::black_box(decoded.payload_bytes());
    }
    samples
}

fn measure_transform(
    frame: &FrameLease,
    transform: FrameTransform,
    iterations: usize,
) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let out = transform_packed_frame(frame, transform).expect("transform frame");
        samples.push(start.elapsed());
        std::hint::black_box(out.payload_bytes());
    }
    samples
}

fn percentile(samples: &[Duration], quantile: f64) -> Duration {
    let mut values = samples.to_vec();
    values.sort_unstable();
    let idx = ((values.len().saturating_sub(1)) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    values[idx]
}

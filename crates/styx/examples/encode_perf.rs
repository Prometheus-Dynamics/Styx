#[cfg(feature = "codec-mozjpeg")]
use std::time::{Duration, Instant};

#[cfg(feature = "codec-mozjpeg")]
use styx::prelude::*;

#[cfg(not(feature = "codec-mozjpeg"))]
fn main() {
    eprintln!("Enable the `codec-mozjpeg` feature to run this example.");
}

#[cfg(feature = "codec-mozjpeg")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let encoder = MozjpegEncoder::new(FourCc::new(*b"RG24"), 85);
    let mut samples = Vec::with_capacity(24);

    for _ in 0..24 {
        let frame = build_rg24_frame(1280, 720);
        let start = Instant::now();
        let encoded = encoder.process(frame)?;
        samples.push(start.elapsed());
        std::hint::black_box(encoded.payload_bytes());
    }

    println!(
        "encode_mjpeg_720p p50_ms={:.2} p95_ms={:.2}",
        percentile(&samples, 0.50).as_secs_f64() * 1_000.0,
        percentile(&samples, 0.95).as_secs_f64() * 1_000.0
    );

    Ok(())
}

#[cfg(feature = "codec-mozjpeg")]
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

#[cfg(feature = "codec-mozjpeg")]
fn percentile(samples: &[Duration], quantile: f64) -> Duration {
    let mut values = samples.to_vec();
    values.sort_unstable();
    let idx = ((values.len().saturating_sub(1)) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    values[idx]
}

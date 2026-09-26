//! Perf smoke for the Y8 MJPEG path on real Logitech C270 720p frames.

use std::time::{Duration, Instant};

use styx::prelude::*;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../testing/fixtures/c270_720p_rst.mjpeg"
);
const ITERATIONS: usize = 80;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(FIXTURE)?;
    let starts: Vec<usize> = (0..data.len() - 2)
        .filter(|&i| data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF)
        .collect();
    let frames: Vec<&[u8]> = starts
        .iter()
        .enumerate()
        .map(|(k, &s)| &data[s..*starts.get(k + 1).unwrap_or(&data.len())])
        .collect();

    let decoder = TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
        pyramid_levels: 1,
        ..Default::default()
    });
    let mut samples = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS {
        let frame = mjpeg_frame(frames[i % frames.len()]);
        let start = Instant::now();
        let out = decoder.process(frame)?;
        samples.push(start.elapsed());
        if out.pyramid_level(1).is_none() {
            return Err("missing pyramid level".into());
        }
    }
    report("decode_luma_pyramid_c270_720p", &samples);
    Ok(())
}

fn mjpeg_frame(jpeg: &[u8]) -> FrameLease {
    let pool = BufferPool::with_limits(1, jpeg.len(), 1);
    let mut buf = pool.lease();
    buf.resize(jpeg.len());
    buf.as_mut_slice().copy_from_slice(jpeg);
    let res = Resolution::new(1280, 720).expect("resolution");
    FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 0),
        buf,
        jpeg.len(),
        jpeg.len(),
    )
}

fn report(metric: &str, samples: &[Duration]) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    println!("{metric} p50_ms={:.2} p95_ms={:.2}", at(0.5), at(0.95));
}

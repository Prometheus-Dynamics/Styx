#[cfg(feature = "file-backend")]
use std::fs;
#[cfg(feature = "file-backend")]
use std::path::PathBuf;
#[cfg(feature = "file-backend")]
use std::time::{Duration, Instant};

#[cfg(feature = "file-backend")]
use image::{ImageBuffer, Rgb};
#[cfg(feature = "file-backend")]
use styx::prelude::*;

#[cfg(not(feature = "file-backend"))]
fn main() {
    eprintln!("Enable the `file-backend` feature to run this example.");
}

#[cfg(feature = "file-backend")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_dir()?;
    let paths = write_sample_frames(&dir)?;
    let source = CaptureRequest::file_source(
        FileSourceConfig::new(paths.clone())
            .name("file-replay-perf")
            .fps(30),
    );
    let handle = source.capture_request().start()?;

    let expected_frames = paths.len();
    let mut samples = Vec::with_capacity(expected_frames);
    while samples.len() < expected_frames {
        let start = Instant::now();
        match handle.recv_blocking(Duration::from_millis(50)) {
            RecvOutcome::Data(frame) => {
                samples.push(start.elapsed());
                std::hint::black_box(frame.payload_bytes());
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }

    handle.stop();
    cleanup_temp_dir(&dir);

    if samples.is_empty() {
        return Err("file replay produced no frames".into());
    }

    println!(
        "file_replay frames={} p50_ms={:.2} p95_ms={:.2}",
        samples.len(),
        percentile(&samples, 0.50).as_secs_f64() * 1_000.0,
        percentile(&samples, 0.95).as_secs_f64() * 1_000.0
    );

    Ok(())
}

#[cfg(feature = "file-backend")]
fn write_sample_frames(dir: &std::path::Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    for idx in 0..4u8 {
        let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(640, 360);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = Rgb([
                ((x + idx as u32 * 17) & 0xff) as u8,
                ((y * 3 + idx as u32 * 29) & 0xff) as u8,
                (((x ^ y) + idx as u32 * 11) & 0xff) as u8,
            ]);
        }
        let path = dir.join(format!("frame_{idx:02}.png"));
        img.save(&path)?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(feature = "file-backend")]
fn make_temp_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "styx-file-replay-perf-{}-{}",
        std::process::id(),
        now
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(feature = "file-backend")]
fn cleanup_temp_dir(dir: &std::path::Path) {
    let _ = fs::remove_dir_all(dir);
}

#[cfg(feature = "file-backend")]
fn percentile(samples: &[Duration], quantile: f64) -> Duration {
    let mut values = samples.to_vec();
    values.sort_unstable();
    let idx = ((values.len().saturating_sub(1)) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    values[idx]
}

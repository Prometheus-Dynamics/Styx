//! MCAP recording throughput on a device: frames from a camera service (as a low-priority
//! client: it never restarts or changes the running capture) recorded with
//! `StreamRecorder::mcap` at several rates and chunk compressions, one short run each.
//!
//! ```text
//! record_bench SOCKET DIR [SECONDS] [WIDTHxHEIGHT]
//! ```
//!
//! Luma frames of the size (default 1280x800: the bytes of a RAW8 frame of the OV9782) are
//! taken at the camera's rate and every n-th recorded for 30, 60 and 120 fps; each run writes
//! `DIR/bench.mcap` and deletes it. Per run: frames recorded and dropped (queue full), the rate
//! sustained, MB/s to disk, the compression ratio and this process's CPU per frame (its writer
//! and compression included). Runs stop when the disk under DIR has less than 2 GB free.

use std::time::{Duration, Instant};

use styx::ipc::FrameClient;
use styx::prelude::*;
use styx::replay::{RecordCompression, RecorderOptions, RecordingHeader, StreamRecorder};

const MIN_FREE: u64 = 2 << 30;

fn cpu_time() -> Duration {
    // SAFETY: `getrusage` fills the zeroed struct for the calling process.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

/// Bytes free on the file system holding `dir`.
fn free_bytes(dir: &str) -> u64 {
    let path = std::ffi::CString::new(dir).expect("path");
    // SAFETY: `statvfs` fills the zeroed struct for a valid path.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    s.f_bavail as u64 * s.f_frsize as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(socket), Some(dir)) = (args.first(), args.get(1)) else {
        return Err("usage: record_bench SOCKET DIR [SECONDS] [WIDTHxHEIGHT]".into());
    };
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let (w, h) = args
        .get(3)
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1280u32, 800u32));
    let client = FrameClient::options(socket)
        .low_priority()
        .request(&Frames::gray().size(w, h))?;
    // The camera's rate, from a second of frames.
    let t = Instant::now();
    let mut n = 0u32;
    let mut format = None;
    while t.elapsed() < Duration::from_secs(1) {
        if let RecvOutcome::Data(f) = client.recv(Duration::from_millis(500)) {
            format.get_or_insert(f.meta().format);
            n += 1;
        }
    }
    let format = format.ok_or("no frames from the service")?;
    let camera_fps = f64::from(n) / t.elapsed().as_secs_f64();
    println!("camera: {camera_fps:.1} fps, {format:?}");
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: "record_bench".into(),
            keys: vec![],
        },
        backend: "service".into(),
        format,
        interval: None,
    };
    let path = format!("{dir}/bench.mcap");
    let codecs = [
        ("none", RecordCompression::None),
        ("lz4", RecordCompression::Lz4),
        ("zstd1", RecordCompression::Zstd { level: 1 }),
        ("zstd3", RecordCompression::Zstd { level: 3 }),
    ];
    println!(
        "{:>4} {:>6} {:>8} {:>8} {:>8} {:>7} {:>7} {:>9} {:>9}",
        "fps", "codec", "recorded", "dropped", "fps_out", "MB/s", "ratio", "cpu_ms/f", "maxq_MB"
    );
    for rate in [30u32, 60, 120] {
        let every = (camera_fps / f64::from(rate)).round().max(1.0) as u32;
        for (name, compression) in codecs {
            if free_bytes(dir) < MIN_FREE + seconds * 130 * (1 << 20) {
                println!("stopping: less than 2 GB would stay free under {dir}");
                return Ok(());
            }
            let options = RecorderOptions {
                compression,
                ..Default::default()
            };
            let mut rec = match StreamRecorder::mcap(&path, &header, options) {
                Ok(rec) => rec,
                Err(e) => {
                    println!("{rate:>4} {name:>6} skipped: {e}");
                    continue;
                }
            };
            while let RecvOutcome::Data(_) = client.recv(Duration::ZERO) {}
            let (cpu, start) = (cpu_time(), Instant::now());
            let (mut k, mut offered) = (0u32, 0u64);
            while start.elapsed() < Duration::from_secs(seconds) {
                if let RecvOutcome::Data(f) = client.recv(Duration::from_millis(500)) {
                    if k % every == 0 {
                        rec.record(&f)?;
                        offered += 1;
                    }
                    k += 1;
                }
            }
            let stats = rec.stats();
            // The queue drains into the file before `finish` returns: counted in the time.
            rec.finish()?;
            let took = start.elapsed().as_secs_f64();
            let cpu = (cpu_time() - cpu).as_secs_f64();
            let size = std::fs::metadata(&path).map_or(0, |m| m.len());
            let _ = std::fs::remove_file(&path);
            let written = offered - stats.frames_dropped;
            let raw_bytes = written * u64::from(w) * u64::from(h);
            println!(
                "{rate:>4} {name:>6} {:>8} {:>8} {:>8.1} {:>7.1} {:>7.2} {:>9.2} {:>9.1}",
                written,
                stats.frames_dropped,
                written as f64 / took,
                size as f64 / took / 1e6,
                raw_bytes as f64 / size.max(1) as f64,
                cpu * 1e3 / written.max(1) as f64,
                stats.max_queued_bytes as f64 / 1e6,
            );
        }
    }
    Ok(())
}

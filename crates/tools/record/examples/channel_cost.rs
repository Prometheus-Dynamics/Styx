//! What a data channel costs the recorder.
//!
//! `channel_cost samples <file> [count]`: ns per `Recorder::sample` at 24, 48 and 256 bytes,
//! `count` samples each (default one million), through the recorder's own 1 MiB buffered file.
//!
//! `channel_cost stream <file> <seconds> [imu]`: a 120 fps 1280x800 GREY stream recorded to
//! `<file>` from a producer thread, with a 1 kHz channel of 24-byte samples beside it when
//! `imu` is given. The producer drops a frame when every pool buffer is still queued for the
//! recorder (as a camera drops). `<file>` may be `/dev/null`, to leave the disk out. Prints the recorder thread's CPU time per second of
//! recording, the frames and samples written, and the frames dropped.

use std::fs::File;
use std::io::BufWriter;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use styx_controls::FrameMetadata;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_record::{ChannelSpec, Recorder, RecordingHeader};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const FPS: u64 = 120;
const POOL: usize = 4;

fn thread_cpu_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

fn header(payload: u64) -> RecordingHeader {
    RecordingHeader {
        device: "channel cost".into(),
        keys: vec![],
        backend: "bench".into(),
        format: MediaFormat::with_defaults(FourCc::GREY, Resolution::new(WIDTH, HEIGHT).unwrap()),
        interval: None,
        payload,
    }
}

fn imu() -> ChannelSpec {
    ChannelSpec {
        name: "/imu".into(),
        encoding: "raw".into(),
        ..ChannelSpec::default()
    }
}

fn samples(path: &str, n: u64) {
    for size in [24usize, 48, 256] {
        let mut recorder = Recorder::create(path, &header(4096)).unwrap();
        let id = recorder.declare(imu()).unwrap();
        let data = vec![0x3Cu8; size];
        // Warm the writer's buffer and the caches.
        for i in 0..10_000 {
            recorder.sample(id, i, &data).unwrap();
        }
        let start = Instant::now();
        let cpu = thread_cpu_ns();
        for i in 0..n {
            recorder.sample(id, i, &data).unwrap();
        }
        let cpu = thread_cpu_ns() - cpu;
        let wall = start.elapsed().as_nanos() as u64;
        recorder.finish().unwrap();
        println!(
            "sample {size:>3} B: {:.1} ns wall, {:.1} ns cpu per sample ({} samples)",
            wall as f64 / n as f64,
            cpu as f64 / n as f64,
            n
        );
    }
}

enum Msg {
    Frame(FrameLease<FrameMetadata>),
    Sample(u64, [u8; 24]),
}

fn stream(path: &str, seconds: u64, with_imu: bool) {
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(WIDTH, HEIGHT).unwrap());
    let span = layouts.span().unwrap();
    let pool: FramePool<FrameMetadata> = FramePool::host(POOL, BufferSize::new(span, 0)).unwrap();
    let (tx, rx) = mpsc::sync_channel::<Msg>(4096);
    let end = monotonic_ns() + seconds * 1_000_000_000;

    let path = path.to_owned();
    let recorder = thread::spawn(move || {
        let file = File::create(&path).unwrap();
        let mut recorder = Recorder::new(
            BufWriter::with_capacity(1 << 20, file),
            &header(span as u64),
        )
        .unwrap();
        let id = recorder.declare(imu()).unwrap();
        let cpu = thread_cpu_ns();
        let wall = monotonic_ns();
        for msg in rx {
            match msg {
                Msg::Frame(lease) => recorder.record(&lease).unwrap(),
                Msg::Sample(ts, data) => recorder.sample(id, ts, &data).unwrap(),
            }
        }
        let cpu = thread_cpu_ns() - cpu;
        let wall = monotonic_ns() - wall;
        let (frames, samples) = (recorder.frames(), recorder.samples());
        recorder.finish().unwrap();
        (cpu, wall, frames, samples)
    });

    let imu_thread = with_imu.then(|| {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut next = monotonic_ns();
            let mut n = 0u64;
            while next < end {
                let now = monotonic_ns();
                if now < next {
                    thread::sleep(Duration::from_nanos(next - now));
                }
                let mut data = [0u8; 24];
                data[..8].copy_from_slice(&n.to_le_bytes());
                tx.send(Msg::Sample(next, data)).unwrap();
                n += 1;
                next += 1_000_000;
            }
        })
    });

    let mut dropped = 0u64;
    let mut sent = 0u64;
    let mut next = monotonic_ns();
    let frame_ns = 1_000_000_000 / FPS;
    let mut id = 0u64;
    while next < end {
        let now = monotonic_ns();
        if now < next {
            thread::sleep(Duration::from_nanos(next - now));
        }
        match pool.acquire(FrameEntry::new(id, next, format, layouts)) {
            Ok(mut lease) => {
                lease.write().unwrap().bytes()[0] = id as u8;
                tx.send(Msg::Frame(lease)).unwrap();
                sent += 1;
            }
            Err(_) => dropped += 1,
        }
        id += 1;
        next += frame_ns;
    }
    drop(tx);
    if let Some(t) = imu_thread {
        t.join().unwrap();
    }
    let (cpu, wall, frames, samples) = recorder.join().unwrap();
    println!(
        "stream {WIDTH}x{HEIGHT} GREY {FPS} fps, {seconds} s, imu {}: recorder cpu {:.2}% of a core \
         ({:.1} ms/s), frames {frames}/{} written, {dropped} dropped, samples {samples}",
        if with_imu { "1 kHz x 24 B" } else { "off" },
        cpu as f64 * 100.0 / wall as f64,
        cpu as f64 / (wall as f64 / 1e9) / 1e6,
        sent + dropped,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("samples") => samples(
            &args[2],
            args.get(3).map_or(1_000_000, |n| n.parse().unwrap()),
        ),
        Some("stream") => stream(
            &args[2],
            args[3].parse().unwrap(),
            args.get(4).is_some_and(|a| a == "imu"),
        ),
        _ => {
            eprintln!("usage: channel_cost samples <file> [count] | stream <file> <seconds> [imu]")
        }
    }
}

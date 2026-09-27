//! Perf smoke for shared captures and the frame server on the C270 720p fixture, replayed as
//! fast as it is consumed:
//!
//! - `shared_3x_luma_c270_720p`: three consumers with the same needs (luma at 320x180) each
//!   take a frame; it is decoded once, so this costs about one decode.
//! - `served_luma_c270_320x180`: a planned frame published through a `FrameServer` and received
//!   by a `FrameClient` (copied once into a memfd).

use std::time::{Duration, Instant};

use styx::DeviceIdentity;
use styx::ipc::{FrameClient, FrameServer};
use styx::planner::{plan_frames, plan_many};
use styx::prelude::*;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../testing/fixtures/c270_720p_rst.mjpeg"
);
const ITERATIONS: usize = 80;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let recording = c270_recording()?;
    let result = run(&recording);
    let _ = std::fs::remove_file(recording);
    result
}

fn run(recording: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let detector = FrameRequirements::luma().output_resolution(320, 180);

    let mut consumers = plan_many(
        &replay(recording)?,
        &[detector.clone(), detector.clone(), detector.clone()],
    )?
    .start()?;
    let mut samples = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS + 5 {
        let start = Instant::now();
        for consumer in &mut consumers {
            next(consumer)?;
        }
        if i >= 5 {
            samples.push(start.elapsed());
        }
    }
    drop(consumers);
    report("shared_3x_luma_c270_720p", &samples);

    let path = std::env::temp_dir().join(format!("styx-shared-perf-{}.sock", std::process::id()));
    let server = FrameServer::bind(&path)?;
    let client = FrameClient::connect(&path)?;
    let mut planned = plan_frames(&replay(recording)?, &detector)?.start()?;
    let mut samples = Vec::with_capacity(ITERATIONS);
    while samples.len() < ITERATIONS {
        let frame = next(&mut planned)?;
        let start = Instant::now();
        if server.publish(&frame)? == 0 {
            continue;
        }
        drop(frame);
        match client.recv(Duration::from_secs(2)) {
            RecvOutcome::Data(shared) => {
                std::hint::black_box(shared.planes()[0].data()[0]);
                samples.push(start.elapsed());
            }
            _ => return Err("frame server sent nothing".into()),
        }
    }
    planned.stop();
    report("served_luma_c270_320x180", &samples);
    Ok(())
}

fn next(
    frames: &mut styx::planner::PlannedFrames,
) -> Result<FrameLease, Box<dyn std::error::Error>> {
    for _ in 0..20 {
        match frames.next_frame(Duration::from_millis(500)) {
            RecvOutcome::Data(frame) => return Ok(frame),
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => return Err("replay closed".into()),
        }
    }
    Err("no frame".into())
}

fn replay(recording: &std::path::Path) -> Result<ProbedDevice, Box<dyn std::error::Error>> {
    Ok(CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )?
    .into_device())
}

/// The C270 MJPEG fixture as a replayable 1280x720 camera recording.
fn c270_recording() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let data = std::fs::read(FIXTURE)?;
    let path = std::env::temp_dir().join(format!("styx-shared-perf-{}.mcap", std::process::id()));
    let format = MediaFormat::new(
        FourCc::MJPG,
        Resolution::new(1280, 720).ok_or("resolution")?,
        ColorSpace::Srgb,
    );
    let header = RecordingHeader {
        device: DeviceIdentity {
            display: "c270 fixture".into(),
            keys: vec!["fixture:c270".into()],
        },
        backend: "v4l2".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut recorder = StreamRecorder::with_header(&path, &header)?;
    let starts: Vec<usize> = (0..data.len() - 2)
        .filter(|&i| data[i..i + 3] == [0xFF, 0xD8, 0xFF])
        .collect();
    for (k, &start) in starts.iter().enumerate() {
        let jpeg = &data[start..*starts.get(k + 1).unwrap_or(&data.len())];
        let mut buf = BufferPool::with_limits(1, jpeg.len(), 0).lease();
        buf.resize(jpeg.len());
        buf.as_mut_slice().copy_from_slice(jpeg);
        let meta = FrameMeta::new(format, k as u64 * 33_000_000);
        recorder.record(&FrameLease::single_plane(meta, buf, jpeg.len(), jpeg.len()))?;
    }
    recorder.finish()?;
    Ok(path)
}

fn report(metric: &str, samples: &[Duration]) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    println!("{metric} p50_ms={:.2} p95_ms={:.2}", at(0.5), at(0.95));
}

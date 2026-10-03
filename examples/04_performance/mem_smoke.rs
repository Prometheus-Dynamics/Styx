//! Memory smoke: heap used by capture and decode scenarios that run without hardware.
//!
//! A counting allocator measures every Rust allocation (frame pools, queues, decoded frames)
//! from the start of a scenario: `peak_kb` is the most held at once, `retained_kb` what is
//! still held after everything was stopped and dropped (a leak if it grows). Memory allocated
//! inside C libraries (libjpeg-turbo's working buffers) and memfd frame pools is not on the
//! heap; `rss_peak_kb` covers it: the rise of the process's peak resident size over the
//! scenario (the peak is reset first). `scripts/check-mem-smoke.sh` compares both with
//! `testing/perf/memory-baseline.txt`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use styx::DeviceIdentity;
use styx::planner::{plan_frames, plan_many};
use styx::prelude::*;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        new
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn status_kb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// Reset the process's peak resident size (`VmHWM`) to its current size.
fn reset_rss_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Run `scenario` and print its memory use; `cameras` divides the resident peak into a
/// per-camera share.
fn measure(
    metric: &str,
    cameras: usize,
    scenario: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Let threads of the previous scenario finish freeing.
    std::thread::sleep(Duration::from_millis(50));
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    reset_rss_peak();
    let rss_base = status_kb("VmRSS:");
    scenario()?;
    let rss_peak = status_kb("VmHWM:").saturating_sub(rss_base);
    std::thread::sleep(Duration::from_millis(50));
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    println!(
        "{metric} peak_kb={} retained_kb={} rss_peak_kb={rss_peak} rss_per_camera_kb={}",
        peak / 1024,
        retained / 1024,
        rss_peak / cameras.max(1) as u64,
    );
    Ok(())
}

/// `cameras` virtual 1280x720 cameras captured together, `frames` frames each.
fn virtual_cameras(cameras: usize, frames: usize) -> Result<(), Box<dyn std::error::Error>> {
    let devices: Vec<ProbedDevice> = (0..cameras)
        .map(|i| {
            CaptureRequest::virtual_source(
                VirtualSourceConfig::new()
                    .name(format!("virtual-{i}"))
                    .resolution(1280, 720)
                    .fps(120),
            )
            .into_device()
        })
        .collect();
    let handles = devices
        .iter()
        .map(|device| device.capture_request().start())
        .collect::<Result<Vec<_>, _>>()?;
    for _ in 0..frames {
        for handle in &handles {
            if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(500)) {
                std::hint::black_box(frame.payload_bytes());
            }
        }
    }
    for handle in handles {
        handle.stop();
    }
    Ok(())
}

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../testing/fixtures/c270_720p_rst.mjpeg"
);

/// The C270 MJPEG fixture as a replayable 1280x720 camera recording.
fn c270_recording() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let data = std::fs::read(FIXTURE)?;
    let path = std::env::temp_dir().join(format!("styx-mem-smoke-{}.mcap", std::process::id()));
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

/// Replay the fixture through a plan for `requirements`, `frames` frames.
fn planned_replay(
    recording: &std::path::Path,
    requirements: FrameRequest,
    frames: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )?;
    let plan = plan_frames(source.device(), &requirements)?;
    let mut planned = plan.start()?;
    let mut seen = 0;
    while seen < frames {
        match planned.next_frame(Duration::from_secs(2)) {
            RecvOutcome::Data(frame) => {
                std::hint::black_box(frame.payload_bytes());
                seen += 1;
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => return Err("replay closed".into()),
        }
    }
    planned.stop();
    Ok(())
}

/// Replay the fixture through one shared capture for `requirements` (one consumer each), until
/// every consumer has `frames` frames.
fn shared_replay(
    recording: &std::path::Path,
    requirements: &[FrameRequest],
    frames: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )?;
    let mut consumers = plan_many(source.device(), requirements)?.start()?;
    for _ in 0..frames {
        for consumer in &mut consumers {
            match consumer.next_frame(Duration::from_secs(2)) {
                RecvOutcome::Data(frame) => {
                    std::hint::black_box(frame.payload_bytes());
                }
                RecvOutcome::Empty => {}
                RecvOutcome::Closed => return Err("replay closed".into()),
            }
        }
    }
    Ok(())
}

/// Planned frames published to a client in the same process through a frame server (copied
/// into memfds, released by the client).
fn served_replay(
    recording: &std::path::Path,
    requirements: FrameRequest,
    frames: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )?;
    let path = std::env::temp_dir().join(format!("styx-mem-smoke-{}.sock", std::process::id()));
    let server = styx::ipc::FrameServer::bind(&path)?;
    let client = styx::ipc::FrameClient::connect(&path)?;
    let mut planned = plan_frames(source.device(), &requirements)?.start()?;
    let mut seen = 0;
    while seen < frames {
        if let RecvOutcome::Data(frame) = planned.next_frame(Duration::from_secs(2))
            && server.publish(&frame)? > 0
            && let RecvOutcome::Data(shared) = client.recv(Duration::from_secs(2))
        {
            std::hint::black_box(shared.planes()[0].data()[0]);
            seen += 1;
        }
    }
    planned.stop();
    Ok(())
}

/// A camera service on the replayed fixture with a luma and an RGB client, `frames` frames each
/// (decoded into memfds and passed to the clients without copying).
fn served_camera(
    recording: &std::path::Path,
    frames: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )?
    .into_device();
    let path = std::env::temp_dir().join(format!("styx-mem-service-{}.sock", std::process::id()));
    let service = styx::ipc::CameraService::new(device)
        .keep_streaming()
        .serve(&path)?;
    let clients = [
        Frames::gray().size(320, 180),
        Frames::formats([FourCc::RG24]).size(320, 180),
    ]
    .iter()
    .map(|req| styx::ipc::FrameClient::request(&path, req))
    .collect::<Result<Vec<_>, _>>()?;
    for _ in 0..frames {
        for client in &clients {
            if let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(2)) {
                std::hint::black_box(frame.planes()[0].data()[0]);
            }
        }
    }
    drop(clients);
    service.stop();
    Ok(())
}

const SCENARIOS: [&str; 10] = [
    "mem_virtual_720p_1cam",
    "mem_virtual_720p_4cam",
    "mem_mjpeg_luma_720p",
    "mem_mjpeg_luma_to_320x180",
    "mem_mjpeg_rgb_720p",
    "mem_mjpeg_rgb_to_320x180",
    "mem_shared_luma_and_rgb_to_320x180",
    "mem_shared_3x_luma_to_320x180",
    "mem_served_luma_to_320x180",
    "mem_camera_service_2_clients",
];

fn run(scenario: &str) -> Result<(), Box<dyn std::error::Error>> {
    let luma = Frames::gray();
    let rgb = Frames::formats([FourCc::RG24]);
    let (cameras, requirements) = match scenario {
        "mem_virtual_720p_1cam" => return measure(scenario, 1, || virtual_cameras(1, 60)),
        "mem_virtual_720p_4cam" => return measure(scenario, 4, || virtual_cameras(4, 60)),
        "mem_mjpeg_luma_720p" => (1, luma),
        "mem_mjpeg_luma_to_320x180" => (1, luma.size(320, 180)),
        "mem_mjpeg_rgb_720p" => (1, rgb),
        "mem_mjpeg_rgb_to_320x180" => (1, rgb.size(320, 180)),
        "mem_shared_luma_and_rgb_to_320x180" | "mem_shared_3x_luma_to_320x180" => {
            let small = luma.size(320, 180);
            let consumers = if scenario == "mem_shared_3x_luma_to_320x180" {
                // Decoded once for all three.
                vec![small.clone(), small.clone(), small]
            } else {
                vec![small, rgb.size(320, 180)]
            };
            let recording = c270_recording()?;
            let result = measure(scenario, 1, || shared_replay(&recording, &consumers, 60));
            let _ = std::fs::remove_file(recording);
            return result;
        }
        "mem_camera_service_2_clients" => {
            let recording = c270_recording()?;
            let result = measure(scenario, 1, || served_camera(&recording, 60));
            let _ = std::fs::remove_file(recording);
            return result;
        }
        "mem_served_luma_to_320x180" => {
            let recording = c270_recording()?;
            let result = measure(scenario, 1, || {
                served_replay(&recording, luma.size(320, 180), 60)
            });
            let _ = std::fs::remove_file(recording);
            return result;
        }
        other => return Err(format!("unknown scenario {other}").into()),
    };
    let recording = c270_recording()?;
    let result = measure(scenario, cameras, || {
        planned_replay(&recording, requirements, 60)
    });
    let _ = std::fs::remove_file(recording);
    result
}

/// Each scenario runs in a fresh process, so memory freed by one does not hide the next one's
/// resident growth.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(scenario) = std::env::args().nth(1) {
        return run(&scenario);
    }
    let exe = std::env::current_exe()?;
    for scenario in SCENARIOS {
        let output = std::process::Command::new(&exe).arg(scenario).output()?;
        if !output.status.success() {
            return Err(format!(
                "{scenario} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    Ok(())
}

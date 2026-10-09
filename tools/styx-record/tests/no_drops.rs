//! The recorder never makes the camera drop frames while the disk keeps up: a replayed 30 fps
//! camera with only the delivery queue as slack (8 frames in every-frame mode, 1 in latest), a
//! camera whose controls take 400 ms to read (libcamera answers each control read between
//! frames: 24 controls held the CM5 recorder's loop for ~0.4 s every second), and a disk that
//! writes in bursts with pauses but keeps up on average.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use styx::prelude::*;
use styx_record::source::{Control, ControlReader, DirectSource, Next, Source, SourceInfo};
use styx_record::{Config, Mode as RecMode};

const W: u32 = 320;
const H: u32 = 240;
/// Frames in the replay (contiguous sequence numbers: any gap is a drop).
const FRAMES: u32 = 75;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-record-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn y_plane(seq: u32) -> Vec<u8> {
    (0..W * H)
        .map(|i| (i.wrapping_mul(13) ^ seq.wrapping_mul(29)) as u8)
        .collect()
}

/// A replayed NV12 camera at 30 fps (real time) whose frames carry sequence numbers 0..FRAMES.
fn camera(dir: &Path) -> ProbedDevice {
    let path = dir.join("camera.styxrec");
    let format = MediaFormat::new(
        FourCc::NV12,
        Resolution::new(W, H).unwrap(),
        ColorSpace::Srgb,
    );
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: "ov9782 replay".into(),
            keys: vec![],
        },
        backend: "native".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut rec = StreamRecorder::with_format(&path, &header, StreamFormat::Styxrec).unwrap();
    for seq in 0..FRAMES {
        let mut bytes = y_plane(seq);
        bytes.extend(std::iter::repeat_n(128u8, (W * H / 2) as usize));
        let mut frame =
            FrameLease::from_visible_bytes(format, u64::from(seq) * 33_333_333 + 1, &bytes)
                .unwrap();
        frame.meta_mut().clock = Some(TimestampClock::StreamRelative);
        frame.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence: seq,
            frame_duration_ns: 33_333_333,
            ..NativeFrameMeta::default()
        }));
        rec.record(&frame).unwrap();
    }
    rec.finish().unwrap();
    ReplaySourceConfig::new(&path)
        .pacing(ReplayPacing::Realtime)
        .into_device()
        .unwrap()
}

/// A camera whose controls are slow to read (24 libcamera controls, a frame period each).
struct SlowControls(Box<dyn Source>);

struct SlowReader(Box<dyn ControlReader>);

impl ControlReader for SlowReader {
    fn read(&mut self) -> Result<Vec<Control>, String> {
        std::thread::sleep(Duration::from_millis(400));
        self.0.read()
    }
}

impl Source for SlowControls {
    fn info(&self) -> &SourceInfo {
        self.0.info()
    }

    fn next(&mut self, wait: Duration) -> Next {
        self.0.next(wait)
    }

    fn control_reader(&mut self) -> Box<dyn ControlReader> {
        Box::new(SlowReader(self.0.control_reader()))
    }
}

/// The "disk": a FIFO read a frame at a time with a pause after each (0.7 of the frame
/// period) and a 300 ms stall now and then, so the writer's queue fills and drains.
fn slow_disk(raw_path: &Path) -> std::thread::JoinHandle<Vec<u8>> {
    std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
    let c = std::ffi::CString::new(raw_path.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo on a path we own; open of that FIFO for reading without blocking.
    let fd = unsafe {
        assert_eq!(libc::mkfifo(c.as_ptr(), 0o600), 0);
        libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK)
    };
    assert!(fd >= 0);
    std::thread::spawn(move || {
        // SAFETY: `fd` is ours; blocking reads from here on.
        let mut pipe = unsafe {
            libc::fcntl(fd, libc::F_SETFL, 0);
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd)
        };
        let mut all = Vec::new();
        let mut chunk = vec![0u8; (W * H) as usize];
        let mut frames = 0;
        loop {
            match std::io::Read::read(&mut pipe, &mut chunk) {
                // No writer yet (the recorder opens the file at its first frame).
                Ok(0) if all.is_empty() => std::thread::sleep(Duration::from_millis(5)),
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&chunk[..n]),
                Err(err) => panic!("{err}"),
            }
            if all.len() >= (frames + 1) * (W * H) as usize {
                frames += 1;
                std::thread::sleep(if frames % 20 == 0 {
                    Duration::from_millis(300)
                } else {
                    Duration::from_millis(23)
                });
            }
        }
        all
    })
}

fn record(mode: RecMode, name: &str) {
    let dir = tmp(name);
    let mut cfg = Config::new(dir.join("unused.sock"), dir.join("rec"));
    cfg.source = styx_record::SourceKind::Direct;
    cfg.mode = mode;
    cfg.frames = Some(u64::from(FRAMES) - 5);
    cfg.seconds = Some(20.0);
    cfg.force = true;
    cfg.settings_poll = Duration::from_millis(200);
    let disk = slow_disk(&dir.join("rec").join(format!("rec_{W}x{H}_gray.raw")));
    let direct = DirectSource::open_device(&cfg, camera(&dir)).unwrap();
    let source = SlowControls(Box::new(direct));
    let summary =
        styx_record::record(&cfg, Box::new(source), &AtomicBool::new(false), None).unwrap();
    let raw = disk.join().unwrap();

    assert_eq!(summary.stop_reason, "frame count reached", "{summary:?}");
    assert_eq!(summary.writer_dropped, 0, "the disk kept up: {summary:?}");
    assert_eq!(
        summary.dropped, 0,
        "{mode:?}: camera frames dropped: {summary:?}"
    );
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!(summary.frames, cfg.frames.unwrap());
    // Every frame, in order, byte-exact.
    let size = (W * H) as usize;
    assert_eq!(raw.len(), summary.frames as usize * size);
    let first: u32 = std::fs::read_to_string(&summary.csv_path)
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .split(',')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    for (i, frame) in raw.chunks(size).enumerate() {
        assert!(
            frame == y_plane(first + i as u32),
            "frame {i} is not sequence {}",
            first + i as u32
        );
    }
    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    assert!(
        json.contains("\"source_stream\": \"NV12 Y plane\""),
        "{json}"
    );
}

#[test]
fn every_frame_mode_drops_nothing_with_slow_controls_and_a_bursty_disk() {
    record(RecMode::Every, "nodrop-every");
}

#[test]
fn latest_mode_drops_nothing_while_the_recorder_keeps_up() {
    record(RecMode::Latest, "nodrop-latest");
}

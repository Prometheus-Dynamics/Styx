//! `styx-record` against cameras served by a Styx camera service in this process (clients
//! over the same Unix sockets as between processes): a replayed camera with known Y planes and
//! sequence gaps, and a virtual camera with controls next to another client.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device_with_controls;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;
use styx_record::source::{DirectSource, ServiceSource};
use styx_record::{Config, Mode as RecMode, StillTrigger, Stills};

const W: u32 = 64;
const H: u32 = 48;
/// Sequence numbers of the recorded camera: 3 frames missing at 5-6 and 12.
const SEQUENCES: [u32; 17] = [0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 13, 14, 15, 16, 17, 18, 19];

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-record-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The Y plane of the frame with sequence `seq`: every byte depends on the frame and pixel.
fn y_plane(seq: u32) -> Vec<u8> {
    y_plane_sized(seq, W, H)
}

fn y_plane_sized(seq: u32, w: u32, h: u32) -> Vec<u8> {
    (0..w * h)
        .map(|i| (i.wrapping_mul(7) ^ seq.wrapping_mul(31)) as u8)
        .collect()
}

/// A replayable NV12 camera (`.styxrec`) whose frames carry sequence numbers with gaps and the
/// exposure and gain a sensor Styx drives reports, at 30 fps.
fn replay_camera(dir: &Path, loop_forever: bool) -> ProbedDevice {
    replay_camera_with(dir, (W, H), loop_forever, ReplayPacing::Realtime)
}

fn replay_camera_with(
    dir: &Path,
    (w, h): (u32, u32),
    loop_forever: bool,
    pacing: ReplayPacing,
) -> ProbedDevice {
    let path = dir.join("camera.styxrec");
    let format = MediaFormat::new(
        FourCc::NV12,
        Resolution::new(w, h).unwrap(),
        ColorSpace::Srgb,
    );
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: "ov9782 replay".into(),
            keys: vec!["replay:front".into()],
        },
        backend: "native".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut rec = StreamRecorder::with_format(&path, &header, StreamFormat::Styxrec).unwrap();
    for (i, &seq) in SEQUENCES.iter().enumerate() {
        let mut bytes = y_plane_sized(seq, w, h);
        bytes.extend((0..w * h / 2).map(|i| (128 + i % 3) as u8));
        let mut frame =
            FrameLease::from_visible_bytes(format, u64::from(seq) * 33_333_333 + 1, &bytes)
                .unwrap();
        let meta = frame.meta_mut();
        meta.clock = Some(TimestampClock::StreamRelative);
        meta.backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence: seq,
            exposure_ns: 10_000_000 + if i >= 8 { 5_000_000 } else { 0 },
            analog_gain: 2.0,
            digital_gain: 1.0,
            frame_duration_ns: 33_333_333,
            verified: true,
            ..NativeFrameMeta::default()
        }));
        rec.record(&frame).unwrap();
    }
    rec.finish().unwrap();
    ReplaySourceConfig::new(&path)
        .pacing(pacing)
        .loop_forever(loop_forever)
        .into_device()
        .unwrap()
}

fn control(
    id: u32,
    name: &str,
    kind: ControlKind,
    range: (ControlValue, ControlValue),
) -> ControlMeta {
    ControlMeta {
        id: ControlId(id),
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        default: range.0.clone(),
        min: range.0,
        max: range.1,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    }
}

/// A 1280x800 NV12 camera at 30 fps with the standard controls.
fn virtual_camera() -> ProbedDevice {
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(1280, 800).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(30).unwrap(),
    );
    make_virtual_device_with_controls(
        "ov9782 virtual",
        [mode],
        vec![
            control(
                0xF400_0001,
                "exposure_time_us",
                ControlKind::Uint,
                (ControlValue::Uint(10), ControlValue::Uint(33_000)),
            ),
            control(
                0xF400_0002,
                "gain",
                ControlKind::Float,
                (ControlValue::Float(1.0), ControlValue::Float(16.0)),
            ),
            control(
                0xF400_0005,
                "ae_enable",
                ControlKind::Bool,
                (ControlValue::Bool(false), ControlValue::Bool(true)),
            ),
            control(
                0xF400_0006,
                "awb_enable",
                ControlKind::Bool,
                (ControlValue::Bool(false), ControlValue::Bool(true)),
            ),
        ],
    )
}

fn csv_rows(path: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(styx_record::sidecar::CSV_HEADER));
    lines
        .map(|l| l.split(',').map(String::from).collect())
        .collect()
}

fn json_field<'a>(json: &'a str, key: &str) -> &'a str {
    let at = json
        .find(&format!("\"{key}\": "))
        .unwrap_or_else(|| panic!("no {key} in {json}"));
    let rest = &json[at + key.len() + 4..];
    rest[..rest.find([',', '\n']).unwrap()].trim()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = styx_record::sha256::Sha256::default();
    h.update(bytes);
    h.hex()
}

#[test]
fn every_frame_through_the_service_is_byte_exact_with_its_gaps() {
    let dir = tmp("every");
    let socket = dir.join("service.sock");
    let service = CameraService::new(replay_camera(&dir, false))
        .serve(&socket)
        .unwrap();
    let mut cfg = Config::new(&socket, dir.join("rec"));
    cfg.camera = Some("ov9782".into());
    cfg.frames = Some(SEQUENCES.len() as u64);
    cfg.seconds = Some(10.0);
    let summary = styx_record::run(&cfg, &AtomicBool::new(false), None).unwrap();
    assert_eq!(summary.stop_reason, "frame count reached");
    assert_eq!(summary.frames, SEQUENCES.len() as u64);

    // The Eidos raw grey layout: `<name>_<W>x<H>_gray.raw`, Y planes back to back, no header.
    let raw_path = summary.raw_path.clone().unwrap();
    assert_eq!(
        raw_path.file_name().unwrap().to_str().unwrap(),
        format!("rec_{W}x{H}_gray.raw")
    );
    let raw = std::fs::read(&raw_path).unwrap();
    let expected: Vec<u8> = SEQUENCES.iter().flat_map(|&s| y_plane(s)).collect();
    assert_eq!(raw.len(), expected.len());
    assert!(raw == expected, "the Y planes differ");

    // One row per frame: sequence, timestamp, clock, and the frames missing before it.
    let rows = csv_rows(&summary.csv_path);
    assert_eq!(rows.len(), SEQUENCES.len());
    let mut total = 0;
    for (i, (row, &seq)) in rows.iter().zip(&SEQUENCES).enumerate() {
        assert_eq!(row[0], i.to_string());
        assert_eq!(row[1], seq.to_string());
        assert_eq!(row[2], (u64::from(seq) * 33_333_333 + 1).to_string());
        assert_eq!(row[3], "stream");
        let gap = if i == 0 {
            0
        } else {
            seq - SEQUENCES[i - 1] - 1
        };
        total += gap;
        assert_eq!(row[4], gap.to_string(), "row {i}");
        assert_eq!(row[5], if i == 0 { "first" } else { "sequence" });
        assert!(row[6].parse::<u64>().unwrap() > 0);
    }
    assert_eq!(total, 3);
    assert_eq!(summary.dropped, 3);

    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    assert_eq!(json_field(&json, "complete"), "true");
    assert_eq!(json_field(&json, "width"), W.to_string());
    assert_eq!(json_field(&json, "height"), H.to_string());
    assert_eq!(json_field(&json, "frames"), SEQUENCES.len().to_string());
    assert_eq!(json_field(&json, "raw_gray_bytes"), raw.len().to_string());
    assert_eq!(
        json_field(&json, "raw_gray_sha256"),
        format!("\"{}\"", sha256_hex(&raw))
    );
    assert_eq!(json_field(&json, "dropped_frames"), "3");
    assert_eq!(json_field(&json, "drop_gaps"), "2");
    assert_eq!(json_field(&json, "name"), "\"ov9782 replay (replay)\"");
    assert_eq!(json_field(&json, "fps"), "30");
    assert_eq!(json_field(&json, "service_restarts_caused"), "0");
    assert_eq!(json_field(&json, "timestamp_clock"), "\"stream\"");
    assert_eq!(service.stats().restarts, 0);
}

#[test]
fn joining_next_to_another_client_does_not_restart_its_capture() {
    let dir = tmp("join");
    let socket = dir.join("service.sock");
    let service = CameraService::new(virtual_camera()).serve(&socket).unwrap();
    // Another client already streams the camera (a detector taking NV12 at full size).
    let other = FrameClient::request(&socket, &Frames::nv12()).unwrap();
    let first = loop {
        if let RecvOutcome::Data(f) = other.recv(Duration::from_secs(1)) {
            break f.meta().timestamp;
        }
    };
    let setter = FrameClient::options(&socket).controls().unwrap();

    let mut cfg = Config::new(&socket, dir.join("join"));
    cfg.mode = RecMode::Latest;
    cfg.seconds = Some(1.5);
    cfg.settings_poll = Duration::from_millis(100);
    let source = ServiceSource::open(&cfg).unwrap();
    // Change a control while recording: the settings file records it.
    let set = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        setter
            .set_control(ControlId(0xF400_0001), ControlValue::Uint(20_000))
            .unwrap();
    });
    // The other client keeps receiving frames meanwhile.
    let watch = std::thread::spawn(move || {
        let (mut frames, mut last) = (0, first);
        let until = Instant::now() + Duration::from_millis(1400);
        while Instant::now() < until {
            if let RecvOutcome::Data(f) = other.recv(Duration::from_millis(200)) {
                assert!(f.meta().timestamp > last);
                last = f.meta().timestamp;
                frames += 1;
            }
        }
        frames
    });
    let summary =
        styx_record::record(&cfg, Box::new(source), &AtomicBool::new(false), None).unwrap();
    set.join().unwrap();
    let other_frames = watch.join().unwrap();
    assert!(
        other_frames >= 25,
        "the other client got {other_frames} frames"
    );
    assert_eq!(service.stats().restarts, 0, "{:?}", service.stats());
    assert_eq!(summary.restarts_caused, Some(0));
    assert_eq!(summary.stop_reason, "duration reached");
    assert!(summary.frames >= 25, "{summary:?}");
    let raw = std::fs::metadata(summary.raw_path.unwrap()).unwrap().len();
    assert_eq!(raw, summary.frames * 1280 * 800);
    assert_eq!(csv_rows(&summary.csv_path).len() as u64, summary.frames);

    // The settings in effect, the controls, the change, the recorder's identity.
    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    for (key, value) in [
        ("exposure_us", "10"),
        ("analogue_gain", "1"),
        ("ae_enable", "false"),
        ("awb_enable", "false"),
        ("fps", "30"),
        ("frame_duration_us", "33333.333333333336"),
        ("width", "1280"),
        ("height", "800"),
        ("format", "\"R8\""),
        ("frame_format", "\"GREY\""),
        ("source", "\"service\""),
        ("mode", "\"latest\""),
        ("complete", "true"),
    ] {
        assert_eq!(json_field(&json, key), value, "{key} in {json}");
    }
    for key in [
        "styx_commit",
        "start_utc",
        "start_unix_ns",
        "end_utc",
        "plan",
        "socket",
    ] {
        let v = json_field(&json, key);
        assert!(v != "null" && v != "\"\"", "{key}: {v}");
    }
    assert!(
        json.contains("\"name\": \"exposure_time_us\", \"id\": 4093640705, \"value\": 10"),
        "{json}"
    );
    assert!(
        json.contains("\"name\": \"exposure_time_us\", \"value\": 20000, \"from\": \"control\""),
        "{json}"
    );
}

#[test]
fn stills_on_key_and_on_a_timer() {
    let dir = tmp("stills");
    let socket = dir.join("service.sock");
    let _service = CameraService::new(replay_camera(&dir, true))
        .serve(&socket)
        .unwrap();

    // On key: two Enters.
    let mut cfg = Config::new(&socket, dir.join("keys"));
    cfg.stills = Some(Stills {
        count: 2,
        trigger: StillTrigger::Key,
    });
    cfg.seconds = Some(10.0);
    let (tx, rx) = std::sync::mpsc::channel();
    let keys = std::thread::spawn(move || {
        for _ in 0..2 {
            std::thread::sleep(Duration::from_millis(300));
            tx.send(()).unwrap();
        }
        // Keep stdin "open" until the recorder is done.
        std::thread::sleep(Duration::from_secs(2));
    });
    let summary = styx_record::run(&cfg, &AtomicBool::new(false), Some(rx)).unwrap();
    keys.join().unwrap();
    assert_eq!(summary.stop_reason, "stills taken");
    check_stills(&summary, 2);

    // Every 0.2 s: three stills.
    let mut cfg = Config::new(&socket, dir.join("timer"));
    cfg.stills = Some(Stills {
        count: 3,
        trigger: StillTrigger::Every(Duration::from_millis(200)),
    });
    cfg.seconds = Some(10.0);
    let started = Instant::now();
    let summary = styx_record::run(&cfg, &AtomicBool::new(false), None).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(600));
    check_stills(&summary, 3);
}

/// `count` stills: raw Y planes of real frames, PGMs, Eidos `.txt` metadata, one row each.
fn check_stills(summary: &styx_record::Summary, count: usize) {
    assert_eq!(summary.stills.len(), count);
    assert!(summary.raw_path.is_none());
    let rows = csv_rows(&summary.csv_path);
    assert_eq!(rows.len(), count);
    for (i, (still, row)) in summary.stills.iter().zip(&rows).enumerate() {
        let name = still.file_name().unwrap().to_str().unwrap();
        assert!(
            name.ends_with(&format!("_still{i:03}_{W}x{H}.gray.raw")),
            "{name}"
        );
        assert_eq!(row[10], name);
        assert_eq!(row[5], "still");
        let seq: u32 = row[1].parse().unwrap();
        let raw = std::fs::read(still).unwrap();
        assert!(
            raw == y_plane(seq),
            "still {i} is not frame {seq}'s Y plane"
        );
        let stem = name.trim_end_matches(".gray.raw");
        let pgm = std::fs::read(still.with_file_name(format!("{stem}.pgm"))).unwrap();
        let header = format!("P5\n{W} {H}\n255\n");
        assert_eq!(&pgm[..header.len()], header.as_bytes());
        assert!(pgm[header.len()..] == raw[..]);
        let txt = std::fs::read_to_string(still.with_file_name(format!("{stem}.txt"))).unwrap();
        for line in [
            format!("width={W}"),
            format!("height={H}"),
            "format=R8".into(),
            format!("frame_index={seq}"),
            format!("capture_index={i}"),
            format!("raw={stem}.gray.raw"),
        ] {
            assert!(txt.lines().any(|l| l == line), "{line} not in {txt}");
        }
    }
    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    assert_eq!(json_field(&json, "mode"), "\"stills\"");
    assert_eq!(json_field(&json, "frames"), count.to_string());
}

#[test]
fn direct_mode_records_every_frame_with_its_exposure() {
    let dir = tmp("direct");
    let mut cfg = Config::new(dir.join("unused.sock"), dir.join("direct"));
    cfg.source = styx_record::SourceKind::Direct;
    cfg.frames = Some(SEQUENCES.len() as u64);
    cfg.seconds = Some(10.0);
    let source = DirectSource::open_device(&cfg, replay_camera(&dir, false)).unwrap();
    let summary =
        styx_record::record(&cfg, Box::new(source), &AtomicBool::new(false), None).unwrap();
    let raw = std::fs::read(summary.raw_path.unwrap()).unwrap();
    let expected: Vec<u8> = SEQUENCES.iter().flat_map(|&s| y_plane(s)).collect();
    assert!(raw == expected);
    let rows = csv_rows(&summary.csv_path);
    // The frames' own exposure, gain and frame duration.
    assert_eq!(rows[0][7], "10000");
    assert_eq!(rows[8][7], "15000");
    assert_eq!(rows[0][8], "2");
    assert_eq!(rows[0][9], "33333.333");
    assert_eq!(summary.dropped, 3);
    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    assert_eq!(json_field(&json, "exposure_us"), "10000");
    assert_eq!(json_field(&json, "analogue_gain"), "2");
    assert_eq!(json_field(&json, "source"), "\"direct\"");
    assert!(
        json.contains("\"frame\": 8, \"at_ms\"")
            && json.contains("\"value\": 15000, \"from\": \"frame\""),
        "{json}"
    );
}

#[test]
fn a_disk_that_does_not_keep_up_drops_frames_and_says_so() {
    let dir = tmp("slow");
    let (w, h) = (640, 480);
    let mut cfg = Config::new(dir.join("unused.sock"), dir.join("slow"));
    cfg.frames = Some(150);
    cfg.seconds = Some(20.0);
    cfg.queue = 2;
    cfg.force = true;
    // The "disk": a pipe nobody reads for a while.
    let raw_path = dir.join("slow").join(format!("slow_{w}x{h}_gray.raw"));
    std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
    let c = std::ffi::CString::new(raw_path.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo on a path we own; open of that FIFO for reading without blocking.
    let fd = unsafe {
        assert_eq!(libc::mkfifo(c.as_ptr(), 0o600), 0);
        libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK)
    };
    assert!(fd >= 0);
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        // SAFETY: `fd` is ours; blocking reads from here on.
        let mut pipe = unsafe {
            libc::fcntl(fd, libc::F_SETFL, 0);
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd)
        };
        let mut all = Vec::new();
        std::io::Read::read_to_end(&mut pipe, &mut all).unwrap();
        all
    });
    let camera = replay_camera_with(&dir, (w, h), true, ReplayPacing::Unpaced);
    let source = DirectSource::open_device(&cfg, camera).unwrap();
    let summary =
        styx_record::record(&cfg, Box::new(source), &AtomicBool::new(false), None).unwrap();
    let raw = reader.join().unwrap();
    assert!(summary.writer_dropped > 0, "{summary:?}");
    assert!(summary.errors.is_empty(), "{summary:?}");
    let rows = csv_rows(&summary.csv_path);
    assert_eq!(rows.len() as u64, summary.frames);
    assert_eq!(raw.len() as u64, summary.frames * u64::from(w * h));
    // Each row is its frame, and the frames the disk had no room for show up as drops.
    for (i, row) in rows.iter().enumerate() {
        let seq: u32 = row[1].parse().unwrap();
        let at = i * (w * h) as usize;
        assert!(raw[at..at + (w * h) as usize] == y_plane_sized(seq, w, h)[..]);
    }
    let dropped: u64 = rows.iter().map(|r| r[4].parse::<u64>().unwrap()).sum();
    assert!(dropped > 0);
    let json = std::fs::read_to_string(&summary.settings_path).unwrap();
    assert_eq!(
        json_field(&json, "writer_dropped_frames"),
        summary.writer_dropped.to_string()
    );
    assert_eq!(json_field(&json, "complete"), "true");
}

#[test]
fn ctrl_c_finalises_the_files() {
    let dir = tmp("sigint");
    let socket = dir.join("service.sock");
    let _service = CameraService::new(virtual_camera()).serve(&socket).unwrap();
    let out = dir.join("cut");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_styx-record"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .arg("--quiet")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until it records, then interrupt it as Ctrl-C does.
    let csv = out.join("cut.frames.csv");
    let raw_path = out.join("cut_1280x800_gray.raw");
    let until = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&raw_path).map_or(0, |m| m.len()) < 10 * 1280 * 800 {
        assert!(Instant::now() < until, "the recorder did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");

    let json = std::fs::read_to_string(out.join("cut.json")).unwrap();
    assert_eq!(json_field(&json, "complete"), "true");
    assert_eq!(
        json_field(&json, "stop_reason"),
        "\"interrupted (Ctrl-C / SIGTERM)\""
    );
    let frames: u64 = json_field(&json, "frames").parse().unwrap();
    let raw = std::fs::read(&raw_path).unwrap();
    assert!(frames >= 10);
    assert_eq!(raw.len() as u64, frames * 1280 * 800);
    assert_eq!(csv_rows(&csv).len() as u64, frames);
    assert_eq!(
        json_field(&json, "raw_gray_sha256"),
        format!("\"{}\"", sha256_hex(&raw))
    );
    assert!(json_field(&json, "end_utc").starts_with("\"20"));
}

#[test]
fn a_missing_service_and_a_busy_name_are_explained() {
    let dir = tmp("missing");
    let cfg = Config::new(dir.join("nobody.sock"), dir.join("x"));
    let err = styx_record::run(&cfg, &AtomicBool::new(false), None).unwrap_err();
    assert!(err.contains("no Styx camera service"), "{err}");
    assert!(err.contains("--source direct"), "{err}");

    let socket = dir.join("service.sock");
    let _service = CameraService::new(virtual_camera()).serve(&socket).unwrap();
    let mut cfg = Config::new(&socket, dir.join("y"));
    cfg.camera = Some("back".into());
    let err = styx_record::run(&cfg, &AtomicBool::new(false), None).unwrap_err();
    assert!(
        err.contains("no camera \"back\"") && err.contains("ov9782 virtual"),
        "{err}"
    );
}

/// Record a grey image (e.g. an Eidos `.gray.raw` test case) replayed as a 30 fps camera
/// through a camera service, for a round trip through Eidos's raw video reader:
/// `STYX_RECORD_ROUNDTRIP=case.gray.raw,640x480,OUT_DIR cargo test -p styx-record-cli --test
/// record -- --ignored round_trip`, then `eidos_detect_raw_stream --input
/// OUT_DIR/roundtrip_640x480_gray.raw --width 640 --height 480 ...`.
#[test]
#[ignore = "needs STYX_RECORD_ROUNDTRIP=<gray.raw>,<WxH>,<out dir>"]
fn round_trip_recording_of_a_grey_image() {
    let spec = std::env::var("STYX_RECORD_ROUNDTRIP").unwrap();
    let [image, size, out]: [&str; 3] = spec.split(',').collect::<Vec<_>>().try_into().unwrap();
    let (w, h) = size.split_once('x').unwrap();
    let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
    let pixels = std::fs::read(image).unwrap();
    assert_eq!(pixels.len() as u32, w * h);
    let dir = tmp("roundtrip");
    let format = MediaFormat::new(
        FourCc::GREY,
        Resolution::new(w, h).unwrap(),
        ColorSpace::Srgb,
    );
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: "grey image".into(),
            keys: vec![],
        },
        backend: "native".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let path = dir.join("image.styxrec");
    let mut rec = StreamRecorder::with_format(&path, &header, StreamFormat::Styxrec).unwrap();
    for seq in 0..30u32 {
        let mut frame =
            FrameLease::from_visible_bytes(format, u64::from(seq) * 33_333_333 + 1, &pixels)
                .unwrap();
        frame.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence: seq,
            ..NativeFrameMeta::default()
        }));
        rec.record(&frame).unwrap();
    }
    rec.finish().unwrap();
    let camera = ReplaySourceConfig::new(&path)
        .pacing(ReplayPacing::Realtime)
        .into_device()
        .unwrap();
    let socket = dir.join("service.sock");
    let _service = CameraService::new(camera).serve(&socket).unwrap();
    let mut cfg = Config::new(&socket, PathBuf::from(out));
    cfg.name = "roundtrip".into();
    cfg.frames = Some(30);
    cfg.seconds = Some(10.0);
    cfg.force = true;
    let summary = styx_record::run(&cfg, &AtomicBool::new(false), None).unwrap();
    let raw = std::fs::read(summary.raw_path.unwrap()).unwrap();
    assert_eq!(raw.len(), 30 * pixels.len());
    assert!(raw.chunks(pixels.len()).all(|f| f == pixels));
}

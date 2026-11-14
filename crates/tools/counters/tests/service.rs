//! The counters on the camera service, on the simulated backend: `lemnosd` in this process on
//! `lemnos-sim`, with the service's own simulator (shared test code, the styx-sim drivers).
//!
//! Each test starts a service with the counting outlets, a counters ring, and reads the snapshots
//! the way another process would.

use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_controls::FrameMetadata;
use styx_counters::{
    CameraState, Counted, DropReason, Health, Hub, Registry, SNAPSHOT_BYTES, Snapshot, Subscriber,
    read_snapshot,
};
use styx_frame::FrameLease;
use styx_native_run::Clock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Listers, Outlet, Outlets, Service, camera_socket};
use styx_socket::Reader;

const CAM0: &str = "lemnos:cam0-sensor";
const CAM1: &str = "lemnos:cam1-sensor";
/// The sensor's frame length in lines in the 120 fps mode (320 by 40) that the request of 60 fps
/// selects, and the line time in nanoseconds. The mode's frame length is its frame time over the
/// line time, rounded down (`styx-sim`).
const FRAME_LENGTH_LINES: u64 = 915;
const LINE_TIME_NS: u64 = 9_100;
const CHILD_SOCKET: &str = "STYX_COUNTERS_CHILD_SOCKET";
const CHILD_OUT: &str = "STYX_COUNTERS_CHILD_OUT";
const CHILD_COUNT: &str = "STYX_COUNTERS_CHILD_COUNT";

/// Two complete chains (sensor, receiver, ISP each) on one daemon.
fn two_cameras() -> String {
    "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\nconfig = { modes = \"run\", drop_frames = 2 }\n\
     [[devices]]\nid = \"isp1\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi1\"\ndriver = \"sim-receiver\"\nlink = \"isp1\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi1\"\nconfig = { modes = \"run\", drop_frames = 2 }\n"
        .to_string()
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-counters-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

fn daemon(socket: &Path, text: &str) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-counters-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![
            entry::<SimSensor>(),
            entry::<SimReceiver>(),
            entry::<SimIsp>(),
        ],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    LemnosDaemon::start(config).expect("start lemnosd")
}

/// Records each backoff wait, and pauses briefly so a failing camera does not spin.
#[derive(Default)]
struct Recorder {
    waits: Mutex<Vec<Duration>>,
}

impl Clock for Recorder {
    fn now(&self) -> Duration {
        Duration::ZERO
    }

    fn wait(&self, for_: Duration, _cancel: &AtomicBool) {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(for_);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[derive(Debug)]
struct NoV4l2;

impl V4l2Lister for NoV4l2 {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for NoV4l2 {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

/// Counts the frames each camera's outlets were given. Stands for the service's own way out.
#[derive(Default)]
struct Tally {
    counts: Mutex<BTreeMap<String, Arc<AtomicU64>>>,
}

impl Tally {
    fn count(&self, camera: &str) -> u64 {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(camera)
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }
}

struct TallyOutlet(Arc<AtomicU64>);

impl Outlet for TallyOutlet {
    fn frame(&mut self, _camera: &str, _lease: &FrameLease<FrameMetadata>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl Outlets for Tally {
    fn attach(&self, camera: &str) -> Box<dyn Outlet> {
        let count = Arc::clone(
            self.counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(camera.to_string())
                .or_default(),
        );
        Box::new(TallyOutlet(count))
    }
}

/// A started service, its counting outlets, and its counters ring.
struct Run {
    service: Service,
    hub: Hub,
    tally: Arc<Tally>,
    registry: Arc<Registry>,
    history: Vec<Snapshot>,
    control: PathBuf,
}

impl Run {
    fn start(dir: &Path, lemnos_socket: &Path, cameras: &str) -> Self {
        let text = format!(
            "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n{cameras}",
            dir.display(),
            lemnos_socket.display()
        );
        let tally = Arc::new(Tally::default());
        let counted = Counted::new(Arc::clone(&tally) as Arc<dyn Outlets>);
        let registry = counted.registry();
        let deps = Deps {
            clock: Arc::new(Recorder::default()),
            listers: Listers {
                native: Arc::new(styx_service::Daemon {
                    socket: lemnos_socket.to_path_buf(),
                }),
                libcamera: Arc::new(NoV4l2),
                v4l2: Arc::new(NoV4l2),
            },
            outlets: Arc::new(counted),
        };
        let service =
            Service::start(Config::parse(&text).expect("config"), deps).expect("start the service");
        let control = service.control_socket().to_path_buf();
        let hub =
            Hub::bind(Arc::clone(&registry), &dir.join("counters.sock")).expect("counters ring");
        Self {
            service,
            hub,
            tally,
            registry,
            history: Vec::new(),
            control,
        }
    }

    /// One pass of the publisher: accepts readers, builds and publishes every camera's snapshot.
    fn tick(&mut self) -> Vec<Snapshot> {
        let statuses = self.service.statuses();
        let built = self
            .hub
            .tick(&statuses)
            .expect("the counters ring publishes");
        self.history.extend(built.iter().copied());
        built
    }

    fn latest(&self, camera: &str) -> Option<Snapshot> {
        self.history
            .iter()
            .rev()
            .find(|s| s.camera.as_str() == camera)
            .copied()
    }

    /// Ticks until `ok` holds on the statuses, or fails at `within`.
    fn until(&mut self, what: &str, within: Duration, mut ok: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + within;
        loop {
            self.tick();
            if ok(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn state(&self, camera: &str) -> Option<styx_service::slot::State> {
        self.service
            .statuses()
            .into_iter()
            .find(|s| s.id == camera)
            .map(|s| s.state)
    }

    fn frames(&self, camera: &str) -> u64 {
        self.service
            .statuses()
            .into_iter()
            .find(|s| s.id == camera)
            .map_or(0, |s| s.frames)
    }
}

/// Sends one line to the control socket and reads the answer through its `ok` or `err` line.
fn ask(control: &Path, line: &str) -> Vec<String> {
    let mut stream = UnixStream::connect(control).expect("connect to the control socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    writeln!(stream, "{line}").expect("send");
    let mut reader = BufReader::new(stream);
    let mut out = Vec::new();
    loop {
        let mut answer = String::new();
        let n = reader.read_line(&mut answer).expect("read the answer");
        assert!(n > 0, "the control socket closed before answering {line:?}");
        let answer = answer.trim_end().to_string();
        let last = answer == "ok" || answer.starts_with("err");
        out.push(answer);
        if last {
            return out;
        }
    }
}

fn copies() -> u64 {
    styx_counters::copies().0
}

#[test]
fn counters_move_as_a_streaming_camera_delivers_frames() {
    let dir = test_dir("move");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let mut run = Run::start(
        &dir,
        &lemnos_socket,
        &format!(
            "[camera.\"{CAM0}\"]\nfps_milli = 60000\nexposure_us = 10000\n\
             [camera.\"{CAM1}\"]\nfps_milli = 60000\nexposure_us = 10000\n"
        ),
    );
    run.registry
        .request(CAM0, 60_000, "fps_milli is the sensor mode's floor");
    run.until("both cameras streaming", Duration::from_secs(30), |r| {
        r.frames(CAM0) > 60 && r.frames(CAM1) > 60
    });
    assert_eq!(ask(&run.control, &format!("stop {CAM0}")), vec!["ok"]);
    run.until("camera 0 stopped", Duration::from_secs(10), |r| {
        r.state(CAM0) == Some(styx_service::slot::State::Stopped)
    });
    run.tick();

    // Every frame delivered is counted once: the counters, the status and the outlet agree.
    let snap = run.latest(CAM0).expect("a snapshot of camera 0");
    let delivered = run.tally.count(CAM0);
    assert_eq!(
        snap.frames, delivered,
        "frames counted exactly as delivered"
    );
    let status_frames = run.frames(CAM0);
    assert_eq!(
        snap.frames, status_frames,
        "the counters and the status agree"
    );
    assert_eq!(
        snap.drops[DropReason::Unattributed as usize],
        0,
        "no identity skipped on a camera that keeps up"
    );
    assert_eq!(snap.copies, 0, "frames were copied");
    assert_eq!(copies(), 0, "the process copied frames");
    assert_eq!(snap.state, CameraState::Stopped);
    assert_eq!(snap.health, Health::Healthy);
    assert_eq!(snap.owner, styx_counters::Owner::Native);
    assert!(
        snap.owner_reason.as_str().contains("native"),
        "the chooser's reason: {:?}",
        snap.owner_reason
    );
    // The request is a floor: the sensor mode runs at 120 fps, so the rates differ and the reason
    // is shown with the request.
    assert_eq!(snap.requested_fps_milli, 60_000);
    assert_eq!(
        snap.requested_reason.as_str(),
        "fps_milli is the sensor mode's floor",
        "the rates differ, so the reason is shown"
    );
    assert!(
        run.history
            .iter()
            .filter(|s| s.camera.as_str() == CAM0)
            .all(|s| s.achieved_fps_milli == 0 || s.achieved_fps_milli > 115_000),
        "the stream runs at the mode's 120 fps"
    );
    let other = run.latest(CAM1).expect("a snapshot of camera 1");
    assert!(
        other.frames > 60,
        "camera 1 kept streaming: {}",
        other.frames
    );

    // The achieved rate, window by window: every window with two frames or more spans the sensor's
    // frame length, to within one line.
    let expected_ns = FRAME_LENGTH_LINES * LINE_TIME_NS;
    let mut checked = 0;
    for s in run
        .history
        .iter()
        .filter(|s| s.camera.as_str() == CAM0 && s.window_frames >= 2)
    {
        assert_ne!(s.achieved_fps_milli, 0);
        let interval = 1_000_000_000_000_u64 / u64::from(s.achieved_fps_milli);
        let difference = interval.abs_diff(expected_ns);
        assert!(
            difference <= LINE_TIME_NS,
            "window {}: {interval} ns per frame, sensor {expected_ns} ns, {difference} apart",
            s.seq
        );
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} windows with two frames");
    let final_window = run.latest(CAM0).expect("camera 0");
    assert_eq!(
        final_window.requested_reason.as_str(),
        "fps_milli is the sensor mode's floor"
    );
    println!(
        "camera 0: frames {}, windows checked {checked}, sensor {expected_ns} ns, last window {} milli-fps",
        snap.frames,
        run.latest(CAM0).map_or(0, |s| s.achieved_fps_milli)
    );
    run.service.shutdown();
}

#[test]
fn a_slow_reader_skips_frames_and_the_counters_show_it_is_connected() {
    let dir = test_dir("slow");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let mut run = Run::start(
        &dir,
        &lemnos_socket,
        &format!("[camera.\"{CAM0}\"]\nfps_milli = 60000\n"),
    );
    run.until("camera 0 streaming", Duration::from_secs(30), |r| {
        r.frames(CAM0) > 10
    });
    let socket = camera_socket(&dir, CAM0);
    run.until("camera 0 publishing", Duration::from_secs(10), |r| {
        r.latest(CAM0)
            .is_some_and(|s| s.publish == styx_counters::PublishState::Publishing)
    });
    let mut reader = loop {
        match UnixStream::connect(&socket)
            .and_then(|s| Reader::connect(s).map_err(|e| std::io::Error::other(e.to_string())))
        {
            Ok(reader) => break reader,
            Err(_) => {
                run.tick();
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    // The reader does not look for a while: the camera keeps publishing, and the ring overwrites.
    let before = run.frames(CAM0);
    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        run.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    let delivered_meanwhile = run.frames(CAM0) - before;
    assert!(
        delivered_meanwhile > 60,
        "camera 0 delivered {delivered_meanwhile} while the reader slept"
    );
    let snap = run
        .tick()
        .into_iter()
        .find(|s| s.camera.as_str() == CAM0)
        .expect("camera 0");
    assert_eq!(snap.readers, 1, "the counters show one reader connected");
    assert_eq!(snap.publish, styx_counters::PublishState::Publishing);
    // The reader then takes what it can: the frames it missed are skipped.
    let mut got = 0;
    while let Ok(Some(frame)) = reader.next_frame() {
        drop(frame);
        got += 1;
    }
    let stats = reader.stats();
    assert!(
        stats.skipped > 0,
        "the slow reader skipped nothing: {stats:?}"
    );
    assert!(got > 0, "the slow reader got nothing");
    println!(
        "slow reader: delivered {got}, skipped {}, camera delivered {delivered_meanwhile} meanwhile, readers {}",
        stats.skipped, snap.readers
    );
    drop(reader);
    run.service.shutdown();
}

#[test]
fn a_camera_that_leaves_and_returns_shows_backoff_and_then_streaming() {
    let dir = test_dir("leave");
    let lemnos_socket = dir.join("lemnos.sock");
    let board = two_cameras();
    let lemnos = daemon(&lemnos_socket, &board);
    let mut run = Run::start(
        &dir,
        &lemnos_socket,
        &format!("[camera.\"{CAM0}\"]\nfps_milli = 60000\n"),
    );
    run.until("camera 0 streaming", Duration::from_secs(30), |r| {
        r.frames(CAM0) > 30
    });
    let before_leaving = run.frames(CAM0);

    // The camera leaves: the daemon goes away.
    drop(lemnos);
    run.until("camera 0 backing off", Duration::from_secs(30), |r| {
        r.state(CAM0) == Some(styx_service::slot::State::Backoff)
    });
    run.until("a backoff snapshot", Duration::from_secs(10), |r| {
        r.latest(CAM0)
            .is_some_and(|s| s.state == CameraState::Backoff)
    });
    let backoff = run.latest(CAM0).expect("backoff snapshot");
    assert!(backoff.next_delay_ms > 0, "{backoff:?}");
    assert!(
        backoff.failures >= 1 && backoff.total_failures >= 1,
        "{backoff:?}"
    );
    assert!(!backoff.last_error.is_empty(), "{backoff:?}");

    // It comes back: the same socket, the same board.
    let _lemnos = daemon(&lemnos_socket, &board);
    run.until("camera 0 streaming again", Duration::from_secs(40), |r| {
        r.state(CAM0) == Some(styx_service::slot::State::Streaming)
            && r.frames(CAM0) > before_leaving + 30
    });
    run.tick();
    let states: Vec<CameraState> = run
        .history
        .iter()
        .filter(|s| s.camera.as_str() == CAM0)
        .map(|s| s.state)
        .collect();
    let first_backoff = states
        .iter()
        .position(|s| *s == CameraState::Backoff)
        .expect("backoff seen");
    assert!(
        states[first_backoff..].contains(&CameraState::Streaming),
        "streaming after the backoff: {states:?}"
    );
    let waits = run.service.statuses();
    assert!(waits.iter().all(|s| s.total_failures < 1000));
    assert_eq!(copies(), 0);
    run.service.shutdown();
}

#[test]
fn a_snapshot_is_read_by_a_reader_in_another_process() {
    if std::env::var(CHILD_SOCKET).is_ok() {
        return; // the child role: see `snapshot_reader_child`
    }
    let dir = test_dir("process");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let mut run = Run::start(
        &dir,
        &lemnos_socket,
        &format!("[camera.\"{CAM0}\"]\nfps_milli = 60000\n"),
    );
    run.until("camera 0 streaming", Duration::from_secs(30), |r| {
        r.frames(CAM0) > 10
    });

    let out = dir.join("child-snapshots.bin");
    let exe = std::env::current_exe().expect("this test binary");
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "snapshot_reader_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, dir.join("counters.sock"))
        .env(CHILD_OUT, &out)
        .env(CHILD_COUNT, "5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the reader process");
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        run.tick();
        if let Some(status) = child.try_wait().expect("wait for the reader process") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the reader process did not finish"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("the reader's output");
    assert!(
        status.success(),
        "reader process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let bytes = std::fs::read(&out).expect("the reader wrote its snapshots");
    assert_eq!(bytes.len(), 5 * SNAPSHOT_BYTES, "five records");
    let mut previous: Option<Snapshot> = None;
    for record in bytes.chunks(SNAPSHOT_BYTES) {
        let snap = read_snapshot(record).expect("a snapshot the other process can read");
        assert_eq!(snap.camera.as_str(), CAM0);
        assert_eq!(snap.copies, 0);
        if let Some(prev) = previous {
            assert!(snap.seq > prev.seq, "snapshot numbers grow");
            assert!(snap.frames >= prev.frames, "frames never go back");
        }
        previous = Some(snap);
    }
    let last = previous.expect("snapshots");
    assert!(
        last.frames > 10,
        "the reader saw frames counted: {}",
        last.frames
    );
    println!(
        "reader process: 5 snapshots, seq {}..{}, frames {}..{}",
        previous.map_or(0, |s| s.seq) - 4,
        last.seq,
        bytes_first_frames(&bytes),
        last.frames
    );
    run.service.shutdown();
}

fn bytes_first_frames(bytes: &[u8]) -> u64 {
    read_snapshot(&bytes[..SNAPSHOT_BYTES]).map_or(0, |s| s.frames)
}

/// The child role: connects to the counters ring, reads snapshots, and writes them to a file.
#[test]
fn snapshot_reader_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET) else {
        return;
    };
    let out = PathBuf::from(std::env::var(CHILD_OUT).expect("the output file"));
    let count: usize = std::env::var(CHILD_COUNT)
        .expect("the snapshot count")
        .parse()
        .expect("a number");
    let deadline = Instant::now() + Duration::from_secs(80);
    let mut subscriber = loop {
        match Subscriber::connect(Path::new(&socket)) {
            Ok(subscriber) => break subscriber,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {socket}: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    let mut bytes = Vec::new();
    let mut got = 0;
    while got < count {
        assert!(Instant::now() < deadline, "only {got} snapshots");
        match subscriber.next_snapshot().expect("a snapshot") {
            // The service has every camera it lists: this reader takes camera 0's.
            Some(snap) if snap.camera.as_str() == CAM0 => {
                bytes.extend_from_slice(&snap.to_bytes());
                got += 1;
            }
            Some(_) => {}
            None => {
                let _ = subscriber.wait(Duration::from_millis(100));
            }
        }
    }
    std::fs::write(&out, &bytes).expect("write the snapshots");
    println!("STYX SNAPSHOTS {got}");
}

//! The service's stall and silence reports on a simulated camera (`SimRig`, styx-sim): a reader
//! attached to a camera's socket stays attached through a failure, is told of the stall and of its
//! return in order, and the status line says retrying and then running again.
//!
//! The service runs in this process against the rig's daemon. Every wait has a deadline and fails
//! saying what it waited for.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_native_run::SystemClock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Service, camera_socket};
use styx_sim::{Fault, SimRig};
use styx_socket::{Carried, ProducerState, Reader};

/// The sensor's reason for its failure, as the device gives it.
const REASON: &str = "bus error while streaming: restarting the sensor";
/// The rig's camera, as the service names it on its status line.
const CAM: &str = "lemnos:cam0-sensor";
const WAIT: Duration = Duration::from_secs(30);

/// What a reader was told, in the order it was told.
#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Frame(u64),
    Stalled(ProducerState),
    Resumed(Carried),
}

fn frames(events: &[Ev]) -> usize {
    events.iter().filter(|e| matches!(e, Ev::Frame(_))).count()
}

fn test_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("styx-service-stall-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

#[derive(Debug)]
struct NoLister;

impl V4l2Lister for NoLister {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for NoLister {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

/// The rig's daemon and the service on it, with the camera streaming.
struct Fixture {
    rig: SimRig,
    service: Service,
    dir: PathBuf,
    control: PathBuf,
}

impl Fixture {
    fn start(name: &str) -> Self {
        let dir = test_dir(name);
        let rig = SimRig::start().expect("the simulated camera");
        let text = format!(
            "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
             [camera.\"{CAM}\"]\nfps_milli = 60000\n",
            dir.display(),
            rig.socket().display()
        );
        let deps = Deps {
            clock: Arc::new(SystemClock::default()),
            listers: Listers {
                native: Arc::new(styx_service::Daemon {
                    socket: rig.socket().to_path_buf(),
                }),
                libcamera: Arc::new(NoLister),
                v4l2: Arc::new(NoLister),
            },
            outlets: Arc::new(Discard),
        };
        let config = Config::parse(&text).expect("config");
        let service = Service::start(config, deps).expect("start the service");
        let control = service.control_socket().to_path_buf();
        let fixture = Self {
            rig,
            service,
            dir,
            control,
        };
        fixture.wait("streaming", |line| {
            field(line, "state") == Some("streaming")
        });
        fixture
    }

    fn camera_socket(&self) -> PathBuf {
        camera_socket(&self.dir, CAM)
    }

    /// The camera's status line, if the service lists it.
    fn line_if_listed(&self) -> Option<String> {
        camera_lines(&self.control)
            .into_iter()
            .find(|l| l.split_ascii_whitespace().nth(1) == Some(CAM))
    }

    /// The camera's status line, now.
    fn line(&self) -> String {
        self.line_if_listed().unwrap_or_else(|| {
            panic!(
                "the service does not list {CAM}; its list: {:?}",
                ask(&self.control, "list")
            )
        })
    }

    /// Waits until the camera's status line satisfies `ok`, or fails naming `what`.
    fn wait(&self, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let line = self.line_if_listed().unwrap_or_default();
            if ok(&line) {
                return line;
            }
            assert!(
                Instant::now() < deadline,
                "waited {WAIT:?} for {what}; the status line is: {line}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(self) {
        self.service.shutdown();
    }
}

/// Sends one request and reads its answer lines (through the `ok` or `err` line).
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

fn camera_lines(control: &Path) -> Vec<String> {
    ask(control, "list")
        .into_iter()
        .filter(|l| l.starts_with("camera "))
        .collect()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace()
        .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// The rest of the line after `key=`: the status line ends with `last_error=`, whose text has spaces.
fn rest<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_once(&format!("{key}=")).map(|(_, v)| v)
}

fn number(line: &str, key: &str) -> u64 {
    field(line, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} in {line}"))
}

/// Connects a reader to the camera's socket once it publishes (before the first frame it refuses).
fn connect(socket: &Path) -> Reader {
    let deadline = Instant::now() + WAIT;
    loop {
        let attempt = UnixStream::connect(socket)
            .map_err(|e| e.to_string())
            .and_then(|s| Reader::connect(s).map_err(|e| e.to_string()));
        match attempt {
            Ok(reader) => return reader,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "waited {WAIT:?} to connect to {}: {e}",
                    socket.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Takes the reader's events until `done` accepts them. The reader is the same object throughout.
fn events(reader: &mut Reader, what: &str, mut done: impl FnMut(&[Ev]) -> bool) -> Vec<Ev> {
    let deadline = Instant::now() + WAIT;
    let mut out = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "waited {WAIT:?} for {what}; events so far: {out:?}"
        );
        match reader.next_event().expect("the stream is not ended") {
            styx_socket::Next::Frame(frame) => out.push(Ev::Frame(frame.lease().id())),
            styx_socket::Next::Stalled(state) => out.push(Ev::Stalled(state)),
            styx_socket::Next::Resumed { state } => out.push(Ev::Resumed(state)),
            styx_socket::Next::Idle => {
                let _ = reader.wait(Some(Duration::from_millis(20)));
            }
        }
        if done(&out) {
            return out;
        }
    }
}

/// Takes the reader's events for `for_`, and returns them.
fn events_for(reader: &mut Reader, for_: Duration) -> Vec<Ev> {
    let until = Instant::now() + for_;
    let mut out = Vec::new();
    while Instant::now() < until {
        match reader.next_event().expect("the stream is not ended") {
            styx_socket::Next::Frame(frame) => out.push(Ev::Frame(frame.lease().id())),
            styx_socket::Next::Stalled(state) => out.push(Ev::Stalled(state)),
            styx_socket::Next::Resumed { state } => out.push(Ev::Resumed(state)),
            styx_socket::Next::Idle => {
                let _ = reader.wait(Some(Duration::from_millis(20)));
            }
        }
    }
    out
}

fn is_stall(e: &Ev) -> bool {
    matches!(e, Ev::Stalled(_))
}

fn is_return(e: &Ev) -> bool {
    matches!(e, Ev::Resumed(_))
}

#[test]
fn a_reader_stays_attached_through_a_transient_failure_and_is_told_the_stall_then_the_return() {
    let fixture = Fixture::start("transient");
    let mut reader = connect(&fixture.camera_socket());
    let before = events(&mut reader, "frames before the failure", |e| frames(e) >= 5);
    let last_before = match before.last() {
        Some(Ev::Frame(id)) => *id,
        other => panic!("a frame last, got {other:?}"),
    };
    let line0 = fixture.line();
    let starts0 = number(&line0, "starts");

    // The sensor fails. The stream stays attached: one stall, then no frame while it lasts.
    fixture.rig.inject_fault(Some(Fault::Failed(REASON)));
    let stall = events(&mut reader, "the stall", |e| e.iter().any(is_stall));
    assert_eq!(stall.iter().filter(|e| is_stall(e)).count(), 1, "{stall:?}");
    assert!(
        matches!(
            stall.last(),
            Some(Ev::Stalled(ProducerState::Retrying { .. }))
        ),
        "the stall is the last event: {stall:?}"
    );
    let during = fixture.wait("the camera to show retrying", |l| {
        field(l, "producer").is_some_and(|p| p.starts_with("retrying:"))
    });
    let since = field(&during, "producer")
        .and_then(|p| p.strip_prefix("retrying:"))
        .and_then(|s| s.parse::<u64>().ok())
        .expect("a stall time");
    assert!(since > 0, "{during}");
    let text = rest(&during, "last_error").unwrap_or_default();
    assert!(
        text.starts_with("camera failed, retrying since ") && text.ends_with(REASON),
        "the stall names the camera and the device's reason: {during}"
    );
    assert!(
        field(&during, "health").is_some_and(|h| h.starts_with("failed")),
        "a failed device shows as failed health while it retries: {during}"
    );
    let quiet = events_for(&mut reader, Duration::from_millis(500));
    assert!(
        quiet.is_empty(),
        "no frame and no second edge while the camera retries: {quiet:?}"
    );
    // A stall adds no skipped frame: the count is the same at its start and after the quiet time.
    let stalled_skipped = number(&during, "skipped");
    let still = fixture.line();
    assert_eq!(
        field(&still, "producer").map(|p| p.starts_with("retrying:")),
        Some(true),
        "{still}"
    );
    assert_eq!(
        number(&still, "skipped"),
        stalled_skipped,
        "a stall skips no frame: {still}"
    );

    // The sensor is healthy again: the return comes before the first frame after the stall, on
    // the same reader, and the sequence goes on.
    fixture.rig.inject_fault(None);
    let back = events(&mut reader, "the return and frames after it", |e| {
        e.iter().any(is_return) && frames(e) >= 5
    });
    assert_eq!(
        back.first(),
        Some(&Ev::Resumed(Carried::Kept)),
        "the return comes first: {back:?}"
    );
    assert_eq!(back.iter().filter(|e| is_return(e)).count(), 1, "{back:?}");
    let after: Vec<u64> = back
        .iter()
        .filter_map(|e| match e {
            Ev::Frame(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert!(
        after[0] > last_before && after.windows(2).all(|w| w[0] < w[1]),
        "the ids go on: last before the stall {last_before}, after {after:?}"
    );

    let line1 = fixture.wait("the camera to show running", |l| {
        field(l, "producer") == Some("running") && field(l, "state") == Some("streaming")
    });
    assert!(number(&line1, "skipped") >= stalled_skipped, "{line1}");
    assert_eq!(field(&line1, "health"), Some("healthy"), "{line1}");
    assert_eq!(
        number(&line1, "starts"),
        starts0,
        "the session was not reopened: {line1}"
    );
    assert_eq!(number(&line1, "discontinuities"), 0, "{line1}");
    println!(
        "proof: stall_kept reader_frames_before={} frames_after_return={} stalls=1 skipped_in_stall={} starts={}",
        before.len(),
        after.len(),
        stalled_skipped,
        starts0
    );
    fixture.finish();
}

#[test]
fn a_failure_that_comes_back_lost_is_a_discontinuity_and_not_a_gap() {
    let fixture = Fixture::start("lost");
    let mut reader = connect(&fixture.camera_socket());
    events(&mut reader, "frames before the failure", |e| frames(e) >= 5);
    let line0 = fixture.line();
    let (starts0, disc0) = (number(&line0, "starts"), number(&line0, "discontinuities"));

    // The device is bound again (a new instance): the stall, then Lost on the same reader.
    fixture.rig.inject_fault(Some(Fault::Gone(REASON)));
    let stall = events(&mut reader, "the stall", |e| e.iter().any(is_stall));
    assert_eq!(stall.iter().filter(|e| is_stall(e)).count(), 1, "{stall:?}");
    let during = fixture.wait("the camera to show retrying", |l| {
        field(l, "producer").is_some_and(|p| p.starts_with("retrying:"))
    });
    let stalled_skipped = number(&during, "skipped");
    let quiet = events_for(&mut reader, Duration::from_millis(500));
    assert!(
        quiet.is_empty(),
        "no frame while the device is dead: {quiet:?}"
    );
    let still = fixture.line();
    assert_eq!(
        number(&still, "skipped"),
        stalled_skipped,
        "a stall skips no frame: {still}"
    );
    // The device is healthy again: the host's next bind revives it (a dead device stays dead while
    // the fault is in force), and the return follows.
    fixture.rig.inject_fault(None);
    let back = events(&mut reader, "the lost return", |e| e.iter().any(is_return));
    assert_eq!(back, vec![Ev::Resumed(Carried::Lost)], "{back:?}");
    // Frames go on from the same reader.
    let more = events(&mut reader, "frames after the return", |e| frames(e) >= 5);
    assert!(frames(&more) >= 5, "{more:?}");

    let line1 = fixture.wait("the lost return counted", |l| {
        number(l, "discontinuities") == disc0 + 1
    });
    assert!(number(&line1, "skipped") >= stalled_skipped, "{line1}");
    assert_eq!(number(&line1, "starts"), starts0, "{line1}");
    assert_eq!(field(&line1, "producer"), Some("running"), "{line1}");
    println!(
        "proof: stall_lost reader_frames_after={} discontinuities={} skipped_in_stall={}",
        frames(&more),
        disc0 + 1,
        stalled_skipped
    );
    fixture.finish();
}

#[test]
fn a_silent_camera_is_reported_by_name_and_its_return_is_kept_on_the_same_session() {
    let fixture = Fixture::start("silence");
    let mut reader = connect(&fixture.camera_socket());
    events(&mut reader, "frames before the silence", |e| frames(e) >= 5);
    let line0 = fixture.line();
    let starts0 = number(&line0, "starts");

    // The camera stops making frames with no fault: the session stays, and after the bound the
    // status names the silence and the ring's readers are told of the stall.
    fixture.rig.pause();
    let stall = events(&mut reader, "the silence report", |e| {
        e.iter().any(is_stall)
    });
    let Some(Ev::Stalled(ProducerState::Retrying { .. })) = stall.last() else {
        panic!("the silence is a stall the reader sees: {stall:?}");
    };
    let during = fixture.wait("the silence named", |l| {
        rest(l, "last_error").is_some_and(|e| e.starts_with("no frame for "))
    });
    let named = rest(&during, "last_error")
        .and_then(|e| e.strip_prefix("no frame for "))
        .and_then(|e| e.strip_suffix(" ms"))
        .and_then(|e| e.parse::<u64>().ok())
        .expect("the silence in ms");
    assert!(
        named >= 1_000,
        "reported no sooner than the bound: {during}"
    );
    assert!(
        field(&during, "producer").is_some_and(|p| p.starts_with("retrying:")),
        "{during}"
    );
    assert_eq!(number(&during, "starts"), starts0, "no reopen: {during}");

    // The camera makes frames again: the return comes first on the same reader, then frames.
    fixture.rig.run();
    let back = events(&mut reader, "the return and frames after it", |e| {
        e.iter().any(is_return) && frames(e) >= 5
    });
    assert_eq!(back.first(), Some(&Ev::Resumed(Carried::Kept)), "{back:?}");
    let line1 = fixture.wait("the camera to show running", |l| {
        field(l, "producer") == Some("running")
    });
    assert_eq!(
        number(&line1, "starts"),
        starts0,
        "the session was not reopened: {line1}"
    );
    println!(
        "proof: silence named_ms={named} starts={starts0} frames_after_return={}",
        frames(&back)
    );
    fixture.finish();
}

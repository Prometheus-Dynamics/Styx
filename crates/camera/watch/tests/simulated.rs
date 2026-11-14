//! The watcher on the simulated backend: `lemnosd` in this process on `lemnos-sim`, with two
//! simulated cameras. Adding a camera is starting the daemon; removing one is stopping it. A
//! device going off and coming back is `set_power` on its sensor through a second client.

use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as Daemon};
use styx_lemnos::Client;
use styx_watch::{Event, Kind, Watch, WatchConfig};

const TWO_CAMERAS: &str = "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\nconfig = { modes = \"run\", drop_frames = 2 }\n\
     [[devices]]\nid = \"isp1\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi1\"\ndriver = \"sim-receiver\"\nlink = \"isp1\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi1\"\nconfig = { modes = \"run\", drop_frames = 2 }\n";

/// The tests of this file share one process, and the leak test counts the process's descriptors
/// and threads, so each test holds this lock for its whole run.
static SERIAL: Mutex<()> = Mutex::new(());

const CAM0: &str = "lemnos:cam0-sensor";
const CAM1: &str = "lemnos:cam1-sensor";
const NAME: &str = "styx-watch-test";

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-watch-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// lemnosd at `socket` with the board text, as the service tests start it.
fn daemon(socket: &Path) -> Daemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: NAME.to_string(),
        board: parse(TWO_CAMERAS).expect("board"),
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
    Daemon::start(config).expect("start lemnosd")
}

/// A watcher on lemnosd at `socket`, with no sysfs nodes and no uevent socket: only native
/// cameras, so every event is the simulated backend's.
fn watch_of(socket: &Path, dir: &Path) -> Watch {
    let empty = dir.join("no-video");
    fs::create_dir_all(&empty).expect("empty class dir");
    Watch::new(WatchConfig {
        client: Some(Client::connecting(socket, NAME)),
        class_dir: empty,
        dev_dir: dir.join("dev"),
        netlink: false,
        libcamera: false,
    })
}

/// Polls until `done` says so, returning every event seen. Panics with what was seen on timeout.
fn until(
    watch: &mut Watch,
    within: Duration,
    what: &str,
    mut done: impl FnMut(&[Event]) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    while !done(&seen) {
        assert!(
            Instant::now() < deadline,
            "{what}: timed out with {:?}",
            labels(&seen)
        );
        if let Some(event) = watch.poll(Duration::from_millis(100)) {
            seen.push(event);
        }
    }
    seen
}

fn labels(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{:?} {}", e.kind, e.camera.name()))
        .collect()
}

fn has(events: &[Event], kind: Kind, name: &str) -> bool {
    events
        .iter()
        .any(|e| e.kind == kind && e.camera.name() == name)
}

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

fn thread_count() -> usize {
    fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn add_and_remove_give_events_in_order() {
    let _serial = serial();
    let dir = test_dir("order");
    let socket = dir.join("lemnos.sock");
    let mut watch = watch_of(&socket, &dir);
    let lemnos = daemon(&socket);

    // Added: both cameras arrive. Arrivals are in the board's order or any order, but both come
    // before anything leaves.
    let arrived = until(&mut watch, Duration::from_secs(20), "both arrive", |seen| {
        has(seen, Kind::Arrived, CAM0) && has(seen, Kind::Arrived, CAM1)
    });
    assert!(
        !arrived.iter().any(|e| e.kind == Kind::Left),
        "{:?}",
        labels(&arrived)
    );
    assert_eq!(watch.cameras().len(), 2);

    // Removed: lemnosd goes away, both leave, once each.
    drop(lemnos);
    let left = until(&mut watch, Duration::from_secs(20), "both leave", |seen| {
        has(seen, Kind::Left, CAM0) && has(seen, Kind::Left, CAM1)
    });
    assert!(
        !left.iter().any(|e| e.kind == Kind::Arrived),
        "{:?}",
        labels(&left)
    );
    assert_eq!(watch.cameras().len(), 0);

    // Added again on the same socket: they arrive again, after they left.
    let lemnos = daemon(&socket);
    let again = until(&mut watch, Duration::from_secs(20), "both return", |seen| {
        has(seen, Kind::Arrived, CAM0) && has(seen, Kind::Arrived, CAM1)
    });
    assert!(
        !again.iter().any(|e| e.kind == Kind::Left),
        "{:?}",
        labels(&again)
    );
    assert_eq!(watch.lost(), 0);
    drop(lemnos);
    drop(watch);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_device_that_goes_off_and_comes_back_gives_leave_then_arrive() {
    let _serial = serial();
    let dir = test_dir("offon");
    let socket = dir.join("lemnos.sock");
    let mut watch = watch_of(&socket, &dir);
    let lemnos = daemon(&socket);
    until(
        &mut watch,
        Duration::from_secs(20),
        "cam0 arrives",
        |seen| has(seen, Kind::Arrived, CAM0),
    );

    // A second client turns the sensor off, and waits until lemnosd reports it off. Lemnos sends
    // a device's state about once every 250 ms and each send carries the state then, so the off
    // has to last past one look for a client to see it (see the PR text for the sub-look case).
    let control = Client::connect(&socket, "styx-watch-control").expect("control client");
    let sensor = control
        .list()
        .expect("list")
        .into_iter()
        .find(|d| d.name == "cam0-sensor")
        .expect("sensor")
        .index;
    control.set_power(sensor, false).expect("power off");
    let off_since = Instant::now();
    while Instant::now() < off_since + Duration::from_secs(10) {
        let state = control
            .list()
            .expect("list")
            .into_iter()
            .find(|d| d.index == sensor)
            .map(|d| d.state);
        if state == Some(lemnos_client::DeviceState::Off) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(600));
    control.set_power(sensor, true).expect("power on");

    let seen = until(
        &mut watch,
        Duration::from_secs(20),
        "leave then arrive",
        |seen| has(seen, Kind::Left, CAM0) && has(seen, Kind::Arrived, CAM0),
    );
    let cam0: Vec<Kind> = seen
        .iter()
        .filter(|e| e.camera.name() == CAM0)
        .map(|e| e.kind)
        .collect();
    assert_eq!(cam0, vec![Kind::Left, Kind::Arrived], "{:?}", labels(&seen));
    println!("off for over one look: {:?}", labels(&seen));
    drop(control);
    drop(lemnos);
    drop(watch);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cycles_of_add_and_remove_leak_no_fds_or_threads() {
    let _serial = serial();
    let dir = test_dir("leak");
    let socket = dir.join("lemnos.sock");
    let cycle = |watch: &mut Watch| {
        let lemnos = daemon(&socket);
        until(watch, Duration::from_secs(20), "arrive", |seen| {
            has(seen, Kind::Arrived, CAM0) && has(seen, Kind::Arrived, CAM1)
        });
        drop(lemnos);
        until(watch, Duration::from_secs(20), "leave", |seen| {
            has(seen, Kind::Left, CAM0) && has(seen, Kind::Left, CAM1)
        });
    };
    let mut watch = watch_of(&socket, &dir);
    // One warm-up cycle: the first use of each library opens its own descriptors once.
    cycle(&mut watch);
    let fds_before = fd_count();
    let threads_before = thread_count();
    const N: usize = 5;
    for _ in 0..N {
        cycle(&mut watch);
    }
    let fds_after = fd_count();
    let threads_after = thread_count();
    println!(
        "{N} add/remove cycles: fds {fds_before} -> {fds_after}, threads {threads_before} -> {threads_after}"
    );
    drop(watch);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(fds_before, fds_after, "descriptors leaked");
    assert_eq!(threads_before, threads_after, "threads leaked");
}

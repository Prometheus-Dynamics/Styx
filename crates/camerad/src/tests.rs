use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use styx::ProbedDevice;
use styx::capture_api::make_virtual_device;

use crate::{CameraConfig, Config, Daemon, Endpoint, JsonFile, TRANSPORT, socket_path};

/// A synthetic camera with a key of its own (virtual cameras share their first key).
fn camera(name: &str) -> ProbedDevice {
    let mut d = make_virtual_device(name, []);
    d.identity.keys = vec![format!("test:{name}")];
    d
}

fn dir(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("styx-camerad-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Every list a sink was given.
type Lists = Arc<Mutex<Vec<Vec<Endpoint>>>>;

/// A sink that keeps every list it is given.
fn recorder() -> (Lists, impl FnMut(&[Endpoint]) + Send) {
    let lists = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let lists = lists.clone();
        move |e: &[Endpoint]| lists.lock().unwrap().push(e.to_vec())
    };
    (lists, sink)
}

/// Held by every test that starts services, so one test's threads are not counted as
/// another's.
static SERVICES: Mutex<()> = Mutex::new(());

fn services() -> std::sync::MutexGuard<'static, ()> {
    SERVICES.lock().unwrap_or_else(|e| e.into_inner())
}

fn config(socket_dir: PathBuf) -> Config {
    Config {
        socket_dir,
        endpoints_file: None,
        ..Config::default()
    }
}

#[test]
fn cameras_are_served_while_attached_and_published_on_change() {
    let _services = services();
    let d = dir("attach");
    let (lists, sink) = recorder();
    let mut daemon = Daemon::new(config(d.clone()), sink);
    let (a, b) = (camera("cam-a"), camera("cam-b"));
    assert!(daemon.sync(&[a.clone(), b.clone()]).is_empty());
    let (pa, pb) = (socket_path(&d, &a), socket_path(&d, &b));
    assert!(pa.exists() && pb.exists());
    let list = lists.lock().unwrap().last().unwrap().clone();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|e| e.transport == TRANSPORT));
    assert!(list.iter().any(|e| e.socket == pa && e.name == "cam-a"));
    assert!(
        list.iter()
            .all(|e| e.backend == Some(styx::BackendKind::Virtual))
    );
    // The same cameras again: nothing changes, nothing is published.
    daemon.sync(&[a.clone(), b.clone()]);
    assert_eq!(lists.lock().unwrap().len(), 1);
    // b goes: its service stops and its socket is removed.
    daemon.sync(std::slice::from_ref(&a));
    assert!(pa.exists() && !pb.exists());
    assert_eq!(lists.lock().unwrap().last().unwrap().len(), 1);
    daemon.stop_all();
    assert!(!pa.exists());
    assert!(lists.lock().unwrap().last().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_configuration_picks_and_skips_cameras() {
    let _services = services();
    let d = dir("pick");
    let (a, b) = (camera("front-cam"), camera("rear-cam"));
    let skip_rear = Config {
        cameras: vec![CameraConfig {
            matches: "rear".into(),
            skip: true,
            ..Default::default()
        }],
        ..config(d.clone())
    };
    let mut daemon = Daemon::new(skip_rear, crate::endpoints::Nowhere);
    daemon.sync(&[a.clone(), b.clone()]);
    assert_eq!(daemon.endpoints().len(), 1);
    assert_eq!(daemon.endpoints()[0].name, "front-cam");
    drop(daemon);
    let only_rear = Config {
        listed_only: true,
        cameras: vec![CameraConfig {
            matches: b.identity.keys[0].clone(),
            output_buffers: Some(4),
            max_clients: Some(2),
            spare_clients: Some(4),
            ..Default::default()
        }],
        ..config(d.clone())
    };
    let resolved = only_rear.resolve(&b).expect("rear is listed");
    assert_eq!(resolved.capture.backends.native.output_buffers, 4);
    assert_eq!(resolved.max_clients, Some(2));
    assert_eq!(resolved.spare_clients, Some(4));
    assert!(only_rear.resolve(&a).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_camera_back_on_other_nodes_restarts() {
    let _services = services();
    let d = dir("replug");
    let (lists, sink) = recorder();
    let mut daemon = Daemon::new(config(d.clone()), sink);
    let a = camera("cam-a");
    daemon.sync(std::slice::from_ref(&a));
    let mut moved = a.clone();
    moved.backends[0].kind = styx::BackendKind::V4l2;
    daemon.sync(std::slice::from_ref(&moved));
    assert!(socket_path(&d, &a).exists());
    // Stopped and started again on the same socket; the list says the new backend.
    let lists = lists.lock().unwrap();
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[1][0].socket, socket_path(&d, &a));
    assert_eq!(lists[1][0].backend, Some(styx::BackendKind::V4l2));
    drop(lists);
    let _ = std::fs::remove_dir_all(&d);
}

/// The CPUs of the threads named `name` in this process.
fn cpus_of_threads(name: &str) -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .filter(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == name))
        .filter_map(|t| {
            let status = std::fs::read_to_string(t.path().join("status")).ok()?;
            status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                .map(|v| v.trim().to_string())
        })
        .collect()
}

#[test]
fn a_cameras_threads_run_on_its_cpus() {
    let _services = services();
    let d = dir("cpus");
    let cpu = crate::affinity::current_thread_cpus().unwrap()[0];
    let pinned = Config {
        cpus: Some(vec![cpu]),
        ..config(d.clone())
    };
    let mut daemon = Daemon::new(pinned, crate::endpoints::Nowhere);
    assert!(daemon.sync(&[camera("pinned-cam")]).is_empty());
    let cpus = cpus_of_threads("styx-camera-ser");
    assert!(!cpus.is_empty(), "the service's accept thread runs");
    assert!(cpus.iter().all(|c| *c == cpu.to_string()), "{cpus:?}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Two cameras whose first keys are the same (virtual ones, identical cameras without a
/// distinguishing key) each get a socket: the second takes its next key.
#[test]
fn cameras_sharing_a_first_key_get_sockets_of_their_own() {
    let _services = services();
    let d = dir("shared-key");
    let mut daemon = Daemon::new(config(d.clone()), crate::endpoints::Nowhere);
    let (a, b) = (
        make_virtual_device("cam-a", []),
        make_virtual_device("cam-b", []),
    );
    assert_eq!(a.identity.keys[0], b.identity.keys[0]);
    assert!(daemon.sync(&[a, b]).is_empty());
    let sockets: Vec<_> = daemon.endpoints().into_iter().map(|e| e.socket).collect();
    assert_eq!(sockets.len(), 2);
    assert_ne!(sockets[0], sockets[1]);
    assert!(sockets.iter().all(|s| s.exists()));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_endpoint_file_is_replaced_whole() {
    let _services = services();
    let d = dir("file");
    let file = d.join("endpoints.json");
    let mut daemon = Daemon::new(config(d.clone()), JsonFile::new(&file));
    daemon.sync(&[camera("cam-a")]);
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(list[0]["name"], "cam-a");
    assert_eq!(list[0]["transport"], TRANSPORT);
    assert!(!d.join("endpoints.json.tmp").exists());
    daemon.stop_all();
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(list, serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_empty_configuration_file_is_the_default() {
    let c: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(c.socket_dir, PathBuf::from("/run/styx/camerad"));
    assert_eq!(c.socket_mode, Some(0o660));
    let c: Config = serde_json::from_str(
        r#"{"cpus": [0, 1], "spare_clients": 2, "cameras": [{"match": "ov9782", "output_buffers": 3, "spare_clients": 4}]}"#,
    )
    .unwrap();
    assert_eq!(c.cpus, Some(vec![0, 1]));
    assert_eq!(c.cameras[0].output_buffers, Some(3));
    assert_eq!(c.spare_clients, Some(2));
    assert_eq!(c.cameras[0].spare_clients, Some(4));
    assert!(serde_json::from_str::<Config>(r#"{"cpu": [0]}"#).is_err());
}

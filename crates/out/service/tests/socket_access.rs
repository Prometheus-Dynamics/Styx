//! The service's sockets get the configured group and mode, whatever the process's umask. The
//! service is the real binary, a child process started through `sh`, so the umask is set before
//! the service starts (std has no safe way to set it from inside the process). The tests stat the
//! sockets and the directories that hold them, and ask the control socket for its status.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use styx_service::{Config, camera_socket};

/// The camera the config names, so the service makes its frame socket at start.
const CAMERA: &str = "test:cam0";

/// A fresh directory for one test run.
fn test_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("styx-socket-access-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// The fields of `/proc/self/status` for `key` (`Gid:`, `Groups:`, `Uid:`), split on blanks.
fn proc_status(key: &str) -> Vec<u32> {
    let text = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .expect("the field")
        .split_ascii_whitespace()
        .filter_map(|field| field.parse().ok())
        .collect()
}

/// The process's effective group.
fn egid() -> u32 {
    proc_status("Gid:")[1]
}

/// The process's supplementary groups, the effective group excluded.
fn supplementary_groups() -> Vec<u32> {
    proc_status("Groups:")
        .into_iter()
        .filter(|g| *g != egid())
        .collect()
}

fn uid() -> u32 {
    proc_status("Uid:")[1]
}

/// The config text: the socket directory under `dir`, the test camera, and `extra` lines.
fn config_text(dir: &Path, extra: &str) -> String {
    format!(
        "socket_dir = \"{}\"\nscan_ms = 100\ncameras = [\"{CAMERA}\"]\n{extra}\n",
        dir.join("sock").display()
    )
}

/// A running service, killed when dropped.
struct Running {
    child: Child,
    control: PathBuf,
    camera: PathBuf,
    socket_dir: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the service binary with the config text under `umask`. Its stderr goes to `run.log`.
/// Returns once both the control socket and the test camera's socket exist.
fn start(dir: &Path, extra: &str, umask: &str) -> Running {
    let config = dir.join("service.toml");
    std::fs::write(&config, config_text(dir, extra)).expect("write the config");
    let log = dir.join("run.log");
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("umask {umask} && exec \"$0\" \"$1\""))
        .arg(env!("CARGO_BIN_EXE_styx-service"))
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&log).expect("log file"))
        .spawn()
        .expect("start sh");
    let parsed = Config::parse(&config_text(dir, extra)).expect("the test config");
    let socket_dir = parsed.socket_dir.clone();
    let mut running = Running {
        child,
        control: parsed.control_socket(),
        camera: camera_socket(&socket_dir, CAMERA),
        socket_dir,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(running.control.exists() && running.camera.exists()) {
        if let Some(status) = running.child.try_wait().expect("try_wait") {
            panic!(
                "the service exited ({status}) before its sockets existed:\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no sockets in time:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    running
}

/// The permission bits (`0o7777` mask) of a path.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

fn gid(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").gid()
}

/// The control socket's answer to `list`, every line, the `ok` last.
fn list(control: &Path) -> Vec<String> {
    let mut stream = UnixStream::connect(control).expect("connect to the control socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    stream.write_all(b"list\n").expect("send");
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let line = line.expect("a line");
        let done = line == "ok" || line.starts_with("err");
        lines.push(line);
        if done {
            break;
        }
    }
    lines
}

/// Checks the control socket, the camera socket and both directories against the expected modes
/// and group, and the control socket's status line.
fn check_access(running: &Running, socket_mode: u32, dir_mode: u32, group: u32) {
    assert_eq!(mode(&running.control), socket_mode, "control socket mode");
    assert_eq!(mode(&running.camera), socket_mode, "camera socket mode");
    assert_eq!(gid(&running.control), group, "control socket group");
    assert_eq!(gid(&running.camera), group, "camera socket group");
    assert_eq!(mode(&running.socket_dir), dir_mode, "socket directory mode");
    let cameras = running.camera.parent().expect("cameras dir");
    assert_eq!(mode(cameras), dir_mode, "cameras directory mode");
    assert_eq!(gid(cameras), group, "cameras directory group");
    let status = list(&running.control);
    let control = status.first().expect("a status line");
    assert!(
        control.starts_with(&format!(
            "control socket={} mode={socket_mode:04o} ",
            running.control.display()
        )),
        "{control}"
    );
    assert!(control.ends_with(&format!(" gid={group}")), "{control}");
    let camera = status
        .iter()
        .find(|line| line.starts_with("camera "))
        .expect("the camera's line");
    assert!(camera.contains(" empty_frames=0 "), "{camera}");
    assert_eq!(status.last().map(String::as_str), Some("ok"));
}

#[test]
fn the_default_sockets_are_owner_and_group_only_under_any_umask() {
    for umask in ["000", "022", "077"] {
        let dir = test_dir(&format!("default-{umask}"));
        let running = start(&dir, "", umask);
        // Owner and group only: 0660, and the process's own group, whatever the umask.
        check_access(&running, 0o660, 0o750, egid());
    }
}

#[test]
fn socket_mode_666_gives_0666_under_a_restrictive_umask() {
    let dir = test_dir("mode-666");
    let running = start(&dir, "socket_mode = \"666\"", "077");
    check_access(&running, 0o666, 0o755, egid());
}

#[test]
fn socket_mode_600_gives_a_directory_with_no_group_or_other_access() {
    let dir = test_dir("mode-600");
    let running = start(&dir, "socket_mode = \"600\"", "000");
    check_access(&running, 0o600, 0o700, egid());
}

#[test]
fn a_numeric_socket_group_in_the_process_groups_is_applied() {
    let Some(group) = supplementary_groups().first().copied() else {
        println!(
            "skipped: the test process has only its own group ({}), so there is no other group to give the sockets",
            egid()
        );
        return;
    };
    let dir = test_dir("numeric-group");
    let running = start(&dir, &format!("socket_group = {group}\n"), "077");
    check_access(&running, 0o660, 0o750, group);
}

#[test]
fn a_group_name_as_a_string_is_looked_up_and_applied() {
    // The group of this process's effective gid, by name, when /etc/group names it.
    let text = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let Some(name) = text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _ = fields.next()?;
        (fields.next()?.parse::<u32>().ok()? == egid()).then(|| name.to_string())
    }) else {
        println!(
            "skipped: /etc/group has no name for the process's group {}",
            egid()
        );
        return;
    };
    let dir = test_dir("named-group");
    let running = start(&dir, &format!("socket_group = \"{name}\"\n"), "077");
    check_access(&running, 0o660, 0o750, egid());
}

/// Starts the service and waits for it to exit (it must, on a config error). Returns the status
/// and the log.
fn start_expecting_failure(dir: &Path, extra: &str) -> (std::process::ExitStatus, String) {
    let config = dir.join("service.toml");
    std::fs::write(&config, config_text(dir, extra)).expect("write the config");
    let log = dir.join("run.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_styx-service"))
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&log).expect("log file"))
        .spawn()
        .expect("start the service");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return (status, std::fs::read_to_string(&log).unwrap_or_default());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the service did not exit on a bad config");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_unknown_socket_group_name_is_a_start_up_error_naming_the_key() {
    let dir = test_dir("unknown-group");
    let (status, log) = start_expecting_failure(&dir, "socket_group = \"styx-no-such-group-x7\"\n");
    assert!(!status.success(), "{status}");
    assert!(log.contains("socket_group"), "{log}");
    assert!(log.contains("not in /etc/group"), "{log}");
    assert!(!dir.join("sock").join("control.sock").exists());
}

#[test]
fn a_bad_socket_mode_is_a_start_up_error_naming_the_key() {
    let dir = test_dir("bad-mode");
    let (status, log) = start_expecting_failure(&dir, "socket_mode = \"689\"\n");
    assert!(!status.success(), "{status}");
    assert!(log.contains("socket_mode"), "{log}");
}

#[test]
fn a_group_the_user_is_not_in_is_refused_at_start_up() {
    if uid() == 0 {
        println!("skipped: run as root, every group can be given, so there is no refusal to see");
        return;
    }
    // Group 0 (root's) is one this unprivileged test process is not in.
    if proc_status("Groups:").contains(&0) || egid() == 0 {
        println!("skipped: the test process is in group 0");
        return;
    }
    let dir = test_dir("refused-group");
    let (status, log) = start_expecting_failure(&dir, "socket_group = 0\n");
    assert!(!status.success(), "{status}");
    assert!(log.contains("socket_group"), "{log}");
    assert!(log.contains("must be in that group"), "{log}");
}

/// Needs root: the service runs as one user, a reader as another in the socket's group connects,
/// and a reader outside the group is refused. Not run in this change (no root here).
#[test]
#[ignore = "needs root to run the service and the readers as different users; not run in this change"]
fn a_reader_in_the_socket_group_connects_and_one_outside_it_is_refused() {}

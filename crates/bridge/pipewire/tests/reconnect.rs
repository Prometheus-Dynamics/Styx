//! The node over a real PipeWire daemon: the producer goes away mid-stream, and a new producer
//! comes back on the same socket. The node is never restarted: it backs off while the producer is
//! away and connects its stream again with the new producer's buffers. Skips, with the reason
//! printed, when this build has no PipeWire node or no daemon is running.
//!
//! Frames reaching a PipeWire consumer need a linked consumer; this host's session manager does not
//! link video, so that leg is not part of this test (see the PR).

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_pipewire::{Control, LinkState, NodeConfig, NodeStatus, node_built, run};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const BUFFERS: usize = 4;
/// NV12: the luma plane, then the interleaved chroma.
const PAYLOAD: usize = (WIDTH * HEIGHT * 3 / 2) as usize;
const NODE: &str = "styx-reconnect-proof";

fn skip(reason: &str) -> bool {
    println!("SKIPPED: {reason}");
    true
}

/// Why this test cannot run here, if it cannot.
fn missing() -> Option<String> {
    if !node_built() {
        return Some(
            "this build has no PipeWire node (needs libpipewire-0.3 and the `pipewire` feature)"
                .into(),
        );
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let remote = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    if !Path::new(&runtime).join(&remote).exists() {
        return Some("no PipeWire daemon socket under XDG_RUNTIME_DIR".into());
    }
    None
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

fn publish(producer: &mut Producer, id: u64) {
    match producer.begin(entry(id)) {
        Ok(mut lease) => {
            let mut planes = lease
                .write()
                .expect("a new frame's lease is the sole owner");
            let bytes = planes.bytes();
            let luma = (WIDTH * HEIGHT) as usize;
            bytes[..luma].fill(id as u8);
            bytes[luma..].fill(0x80);
            producer.publish(lease).expect("publish");
        }
        Err(BeginError::NoFreeBuffer) => {}
        Err(err) => panic!("begin: {err}"),
    }
}

/// A producer on `socket` at 30 fps until stopped. Stopping drops it: its readers' sockets close,
/// which is what a producer going away looks like to the node.
struct Producer30 {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

fn start_producer(socket: &Path) -> Producer30 {
    let socket = socket.to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = thread::spawn(move || {
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind the producer socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let mut producer = Producer::new(ProducerConfig {
            payload: PAYLOAD,
            buffers: BUFFERS,
            history: 8,
        })
        .expect("producer");
        let mut id = 1_u64;
        while !flag.load(Ordering::Acquire) {
            while let Ok((stream, _)) = listener.accept() {
                stream
                    .set_nonblocking(false)
                    .expect("blocking reader socket");
                let _ = producer.accept(
                    stream,
                    Limits {
                        max_held: 2,
                        max_hold: None,
                    },
                );
            }
            publish(&mut producer, id);
            id += 1;
            thread::sleep(Duration::from_millis(33));
        }
        drop(producer);
        drop(listener);
        let _ = fs::remove_file(&socket);
    });
    Producer30 { stop, thread }
}

impl Producer30 {
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("producer thread");
    }
}

fn wait_for(control: &Control, what: &str, timeout: Duration, ok: impl Fn(&NodeStatus) -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        let status = control.status();
        if ok(&status) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last status {status:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_node_survives_its_producer_and_renegotiates_when_a_new_one_returns() {
    if let Some(reason) = missing() {
        skip(&reason);
        return;
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let socket = dir.join("pw-reconnect.sock");
    let config = NodeConfig {
        name: NODE.into(),
        socket: socket.clone(),
        fps: (30, 1),
    };
    let control = Arc::new(Control::new());

    // The first producer, then the node.
    let first = start_producer(&socket);
    let node = {
        let control = control.clone();
        thread::spawn(move || run(&config, &control))
    };
    wait_for(
        &control,
        "the node streaming",
        Duration::from_secs(15),
        |s| s.link.state == LinkState::Streaming && s.stats.streams == 1,
    );
    println!(
        "proof: streaming from the first producer: {:?}",
        control.status()
    );

    // The producer goes away. The node stays up and backs off.
    first.stop();
    wait_for(
        &control,
        "the node waiting for its producer",
        Duration::from_secs(10),
        |s| s.link.state == LinkState::Waiting && s.link.failures >= 1,
    );
    thread::sleep(Duration::from_millis(2500));
    let waiting = control.status();
    println!("proof: producer gone, node waiting: {waiting:?}");
    assert!(
        waiting.link.failures >= 3,
        "backoff probes while the producer is away"
    );
    assert!(
        waiting.link.last_error.is_some(),
        "the last error is reported"
    );
    assert!(
        !node.is_finished(),
        "the node is still running while its producer is away"
    );

    // A new producer on the same socket. The node learns its buffers and connects the stream again.
    let second = start_producer(&socket);
    wait_for(
        &control,
        "the node streaming from the new producer",
        Duration::from_secs(25),
        |s| s.link.state == LinkState::Streaming && s.link.connects >= 2 && s.stats.streams == 2,
    );
    let after = control.status();
    println!("proof: producer returned, stream renegotiated: {after:?}");
    assert!(!node.is_finished(), "the node never restarted");

    control.stop();
    let stats = node
        .join()
        .expect("node thread")
        .expect("a stop is not an error");
    second.stop();
    println!("proof: node stopped, run stats {stats:?}");
    assert_eq!(stats.streams, 2);
}

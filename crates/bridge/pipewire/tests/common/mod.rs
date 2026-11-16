//! What the consumer proofs share: a producer whose four buffers are one memfd cut at offsets,
//! writing frames whose luma is the frame's id, and the node that offers them to PipeWire.

#![allow(dead_code)]

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_pipewire::{Control, LinkState, NodeConfig, NodeError, NodeStats, node_built, run};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

/// Why no proof with a PipeWire consumer can run here, if none can.
pub fn no_node_or_daemon() -> Option<String> {
    if !node_built() {
        return Some("this build has no PipeWire node (the `pipewire` feature is off)".into());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let remote = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    (!Path::new(&runtime).join(&remote).exists())
        .then(|| "no PipeWire daemon socket under XDG_RUNTIME_DIR".into())
}

/// Whether GStreamer's `pipewiresrc` is installed.
pub fn has_pipewiresrc() -> bool {
    Command::new("gst-inspect-1.0")
        .arg("pipewiresrc")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A running producer and node.
pub struct Rig {
    pub name: String,
    pub control: Arc<Control>,
    /// How many frames the producer has tried to begin, whether or not a buffer was free.
    pub attempts: Arc<AtomicU64>,
    /// How many of those found no free buffer.
    pub no_buffer: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    producer: JoinHandle<()>,
    node: JoinHandle<Result<NodeStats, NodeError>>,
}

fn publish(producer: &mut Producer, id: u64, width: u32, height: u32) -> bool {
    let luma = (width * height) as usize;
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(width, height).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, width, height, None, None).expect("layouts");
    let entry = FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts);
    match producer.begin(entry) {
        Ok(mut lease) => {
            let mut planes = lease.write().expect("a new frame is the sole owner");
            let bytes = planes.bytes();
            bytes[..luma].fill(id as u8);
            bytes[luma..].fill(0x80);
            producer.publish(lease).expect("publish");
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

impl Rig {
    /// Starts a producer of `width`x`height` NV12 frames every `period`, and the node named
    /// `name` on it, and waits until the node streams from the producer.
    pub fn start(name: &str, width: u32, height: u32, period: Duration) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let socket = dir.join(format!("{name}.sock"));
        let stop = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicU64::new(0));
        let no_buffer = Arc::new(AtomicU64::new(0));
        let producer = {
            let (socket, stop) = (socket.clone(), stop.clone());
            let (attempts, no_buffer) = (attempts.clone(), no_buffer.clone());
            thread::spawn(move || {
                let _ = fs::remove_file(&socket);
                let listener = UnixListener::bind(&socket).expect("bind the producer socket");
                listener.set_nonblocking(true).expect("nonblocking");
                let mut producer = Producer::new(ProducerConfig {
                    payload: (width * height) as usize * 3 / 2,
                    buffers: 4,
                    history: 8,
                })
                .expect("producer");
                let mut id = 1_u64;
                while !stop.load(Ordering::Acquire) {
                    while let Ok((stream, _)) = listener.accept() {
                        stream.set_nonblocking(false).expect("blocking");
                        let _ = producer.accept(stream, Limits::default());
                    }
                    if !publish(&mut producer, id, width, height) {
                        no_buffer.fetch_add(1, Ordering::AcqRel);
                    }
                    attempts.fetch_add(1, Ordering::AcqRel);
                    id += 1;
                    thread::sleep(period);
                }
                let _ = fs::remove_file(&socket);
            })
        };
        let control = Arc::new(Control::new());
        let node = {
            let control = control.clone();
            let config = NodeConfig {
                name: name.into(),
                socket,
                fps: (30, 1),
            };
            thread::spawn(move || run(&config, &control))
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while control.status().link.state != LinkState::Streaming {
            assert!(Instant::now() < deadline, "the node never streamed");
            thread::sleep(Duration::from_millis(20));
        }
        Self {
            name: name.into(),
            control,
            attempts,
            no_buffer,
            stop,
            producer,
            node,
        }
    }

    /// Stops the node and the producer; the node's counts as they were when it was stopped.
    pub fn finish(self) -> NodeStats {
        let stats = self.control.status().stats;
        self.control.stop();
        self.stop.store(true, Ordering::Release);
        self.producer.join().expect("producer thread");
        let _ = self.node.join().expect("node thread");
        stats
    }
}

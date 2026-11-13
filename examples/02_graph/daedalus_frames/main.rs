//! A Daedalus graph of Styx frames: frames from a camera's ring, fed into a graph as `styx:frame`
//! values.
//!
//! The camera service publishes its frames on a socket; this program reads them with a
//! `styx_socket::Reader` in another process's place (here, in this one) and turns each frame into
//! a `styx:frame` value. The value holds the frame's buffer while the graph has it, so the ring
//! keeps the buffer until the graph lets it go. The graph is one node: `frame_id` takes the value
//! and gives the frame's id.
//!
//! This needs the `daedalus` feature, which fetches the Daedalus crates:
//!
//! ```text
//! cargo run -p styx-examples --features daedalus --example daedalus_frames -- --sim
//! ```
//!
//! Without the feature the example prints how to turn it on and exits 0.

use std::process::ExitCode;

#[cfg(feature = "daedalus")]
use std::path::PathBuf;
#[cfg(feature = "daedalus")]
use std::time::Duration;
#[cfg(feature = "daedalus")]
use styx_examples::{Lab, SIM_CAMERA, connect_reader, flag, serve, value};

/// How many frames to run through the graph.
#[cfg(feature = "daedalus")]
const FRAMES: usize = 10;

#[cfg(feature = "daedalus")]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("daedalus_frames: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "daedalus"))]
fn main() -> ExitCode {
    println!(
        "daedalus_frames needs the daedalus feature, which fetches the Daedalus crates.\n\
         Run: cargo run -p styx-examples --features daedalus --example daedalus_frames -- --sim"
    );
    ExitCode::SUCCESS
}

#[cfg(feature = "daedalus")]
fn run() -> Result<(), String> {
    let sim = flag("--sim");
    let lab = if sim {
        Lab::sim("daedalus_frames")?
    } else {
        Lab::real(value("--lemnos").map(PathBuf::from))
    };
    let camera = if sim {
        SIM_CAMERA.to_string()
    } else {
        value("--camera").ok_or("name the camera with --camera ID")?
    };
    let served = serve(
        &lab,
        "daedalus_frames",
        &camera,
        value("--format").as_deref(),
    )?;
    let mut reader = connect_reader(&served.socket())?;
    let mut graph = graph::load()?;
    println!("graph loaded: frame_id takes styx:frame from the ring of {camera}");

    let mut count = 0;
    while count < FRAMES {
        let Some(frame) = styx_daedalus::next_value(&mut reader).map_err(|e| e.to_string())? else {
            reader
                .wait(Some(Duration::from_millis(500)))
                .map_err(|e| e.to_string())?;
            continue;
        };
        let (id, memory, buffer) = (frame.id(), frame.memory_kind(), frame.buffer_index());
        let out = graph.feed(frame)?;
        println!("frame id={id} memory={memory:?} buffer={buffer} -> graph frame_id={out}");
        count += 1;
    }
    let stats = reader.stats();
    println!(
        "reader: delivered={} skipped={}",
        stats.delivered, stats.skipped
    );
    Ok(())
}

/// The graph: one host input (`frame`, a `styx:frame`), one node, one host output (`id`).
#[cfg(feature = "daedalus")]
mod graph {
    use std::sync::Arc;

    use daedalus::layers::graph::{
        EdgeDoc, EdgePolicyDoc, GraphBody, NodeDoc, NodeKindDoc, PortRefDoc,
    };
    use daedalus::layers::planner::{Config, plan};
    use daedalus::layers::registry::{NodeRecord, Registry};
    use daedalus::prelude::{LoadOptions, LoadedHost, node};
    use daedalus_backend::Backend;
    use daedalus_cpu::CpuBackend;
    use styx_daedalus::{FRAME_KEY, StyxFrame};

    /// The consumer: the frame's id, read from the value the graph was given.
    #[node(id = "styx_examples.frame_id", inputs(frame), outputs(id))]
    fn frame_id(frame: &StyxFrame) -> u64 {
        frame.id()
    }

    const FRAME_ID: &NodeRecord = &FRAME_ID_NODE;

    fn edge(from: u32, from_port: &str, to: u32, to_port: &str) -> EdgeDoc {
        EdgeDoc {
            from: PortRefDoc {
                node: from,
                port: from_port.to_string(),
            },
            to: PortRefDoc {
                node: to,
                port: to_port.to_string(),
            },
            policy: EdgePolicyDoc::default(),
        }
    }

    /// `host.frame -> frame_id -> host.id`.
    fn body() -> GraphBody {
        let mut host = NodeDoc::new("host");
        host.kind = NodeKindDoc::HostBridge;
        host.port_types
            .insert("frame".to_string(), FRAME_KEY.to_string());
        host.port_types.insert("id".to_string(), "u64".to_string());
        GraphBody {
            nodes: vec![host, NodeDoc::new("styx_examples.frame_id")],
            edges: vec![edge(0, "frame", 1, "frame"), edge(1, "id", 0, "id")],
            groups: Vec::new(),
        }
    }

    /// A loaded graph: the host's inputs and outputs, and one step per frame.
    pub struct Graph {
        host: LoadedHost,
    }

    /// Plans and loads the graph on the CPU backend.
    pub fn load() -> Result<Graph, String> {
        let mut registry = Registry::new();
        registry
            .register_all(&[FRAME_ID])
            .map_err(|e| format!("{e:?}"))?;
        let backends: [&dyn Backend; 1] = [&CpuBackend];
        let planned =
            plan(&body(), &registry, &backends, &Config::default()).map_err(|e| e.to_string())?;
        let host = LoadedHost::load(planned, LoadOptions::default()).map_err(|e| e.to_string())?;
        Ok(Graph { host })
    }

    impl Graph {
        /// Gives the graph one frame and runs one step; returns the id the node made. The value
        /// drops after the step, and with it the ring's hold on the buffer.
        pub fn feed(&mut self, frame: StyxFrame) -> Result<u64, String> {
            let io = self.host.host();
            io.feed_typed("frame", Arc::new(frame))
                .map_err(|e| format!("{e:?}"))?;
            io.step().map_err(|e| format!("{e:?}"))?;
            io.take_copy::<u64>("id")
                .map_err(|e| format!("{e:?}"))?
                .ok_or_else(|| "the graph made no id".to_string())
        }
    }
}

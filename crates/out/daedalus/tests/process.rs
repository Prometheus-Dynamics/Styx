//! A graph in a process of its own takes the camera service's frames from the shared ring, as
//! `styx:frame` values.
//!
//! The test is the service: it owns a [`Producer`] and publishes frames, one at a time. The graph
//! is a child process: this same test binary, run again as `graph_process`, with the socket's path
//! in an environment variable. The child connects as a [`Reader`], takes each frame as a
//! `styx:frame` value, feeds it to a graph with a CPU consumer node, and answers with the frame's
//! id once the graph has run. The service publishes the next frame only after that answer, so
//! the ordering and the buffer returns are the thing under test, not the ring's history.
//!
//! The ring's buffers are memfd (the producer maps its own memfd; `styx-socket` offers no dma-buf
//! pool), so the frames are memfd frames in both processes. The copy counters of `styx-frame` are
//! read in both processes and must be zero.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

mod support;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::DmaBufMemory;
use styx_daedalus::{StyxFrame, dmabuf_memory, next_value};
use styx_frame::{MemoryKind, copy_counters};
use styx_socket::{Limits, Producer, ProducerConfig, Reader};

/// Set in the child's environment to the socket's path. Unset, the child test does nothing.
const CHILD_SOCKET: &str = "STYX_DAEDALUS_GRAPH_PROCESS_SOCKET";
const FRAMES: u64 = 12;
const PAYLOAD: usize = 4096;
const BUFFERS: usize = 3;

/// The consumer: the frame's id.
#[node(id = "styx_test.process_frame_check", inputs(frame), outputs(id))]
fn frame_check(frame: &StyxFrame) -> u64 {
    frame.id()
}

const GRAPH: &str = r#"{
  "format": "daedalus.graph",
  "schema_version": 1,
  "graph": {
    "nodes": [
      {"id": "host", "kind": "host_bridge",
       "port_types": {"frame": "styx:frame", "id": "u64"}},
      {"id": "styx_test.process_frame_check"}
    ],
    "edges": [
      {"from": {"node": 0, "port": "frame"}, "to": {"node": 1, "port": "frame"}},
      {"from": {"node": 1, "port": "id"}, "to": {"node": 0, "port": "id"}}
    ]
  }
}"#;

/// The graph's copies of frame bytes, as the counters of `styx-frame` read them.
fn copies() -> Vec<usize> {
    copy_counters()
        .iter()
        .map(|counter| counter.copies())
        .collect()
}

/// The child: a graph that takes the ring's frames, one at a time, and answers each. Runs only
/// when the parent spawned it with the socket's path set.
#[test]
fn graph_process() {
    let Ok(path) = std::env::var(CHILD_SOCKET) else {
        return;
    };
    let socket = UnixStream::connect(&path).expect("the producer's socket");
    let mut reader = Reader::connect(socket).expect("the ring");

    let mut registry = Registry::new();
    registry
        .register_all(&[&FRAME_CHECK_NODE])
        .expect("the consumer registers");
    let dma: DmaBufMemory = dmabuf_memory();
    let backends: [&dyn daedalus_backend::Backend; 2] = [&CpuBackend, &dma];
    let graph = GraphDoc::parse(GRAPH).expect("the graph parses").graph;
    let planned = plan(&graph, &registry, &backends, &Config::default()).expect("the graph plans");
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    let host = loaded.host();

    for expected in 0..FRAMES {
        let value = loop {
            if let Some(value) = next_value(&mut reader).expect("a frame") {
                break value;
            }
            reader
                .wait(Some(Duration::from_secs(5)))
                .expect("the doorbell rang");
        };
        let (id, memory) = (value.id(), value.memory_kind());
        assert_eq!(id, expected, "frames arrive in order");
        assert_eq!(memory, MemoryKind::Memfd, "the ring's frames are memfd");
        let holds_before = reader.stats().held;
        host.feed_typed("frame", Arc::new(value))
            .expect("the frame is fed");
        host.step_until_settled(Duration::from_secs(10))
            .expect("the graph settles");
        let got = host
            .take_copy::<u64>("id")
            .expect("the id is taken")
            .expect("the consumer's answer arrived");
        assert_eq!(got, id, "the consumer saw the frame it was given");
        // The graph has run and let the value go: the ring's hold went with it.
        println!(
            "ack {id} held_before_graph={holds_before} held_after={}",
            reader.stats().held
        );
    }
    drop(loaded);
    println!(
        "done delivered={} held={} copies={:?}",
        reader.stats().delivered,
        reader.stats().held,
        copies()
    );
}

/// Waits up to ten seconds for the child to connect to the listener.
fn accept_within(listener: &UnixListener, limit: Duration) -> UnixStream {
    listener
        .set_nonblocking(true)
        .expect("a nonblocking listener");
    let until = Instant::now() + limit;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("a blocking stream");
                return stream;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "the graph process never connected");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("accept: {err}"),
        }
    }
}

#[test]
fn a_graph_in_another_process_takes_the_ring_frames_in_order_and_the_buffers_come_back() {
    let copies_before = copies();
    let path: PathBuf =
        std::env::temp_dir().join(format!("styx-daedalus-process-{}.sock", std::process::id()));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("a listener");

    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "graph_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, &path)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the graph process starts");
    let ours = accept_within(&listener, Duration::from_secs(10));
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: BUFFERS,
        history: 8,
    })
    .expect("a producer");
    producer
        .accept(ours, Limits::default())
        .expect("the graph process is attached");
    let mut lines = BufReader::new(child.stdout.take().expect("the child's output")).lines();

    let mut acked = Vec::new();
    for id in 0..FRAMES {
        let mut lease = producer
            .begin(support::entry(id))
            .expect("a free buffer: the graph gave the last one back");
        lease.write().expect("sole owner").bytes().fill(id as u8);
        producer.publish(lease).expect("published");
        // Wait for the graph process to answer this frame before the next one.
        let answer = loop {
            let line = lines
                .next()
                .expect("the graph process is still running")
                .expect("its output");
            // The libtest header ("test graph_process ... ") can share the line with the answer.
            if let Some(at) = line.find("ack ") {
                break line[at..].to_string();
            }
        };
        println!("{answer}");
        let fields: Vec<&str> = answer.split_whitespace().collect();
        let got: u64 = fields[1].parse().expect("the frame's id");
        assert_eq!(got, id);
        assert_eq!(
            fields[3], "held_after=0",
            "the graph let the value go and the ring's hold went with it"
        );
        acked.push(got);
    }
    let mut done = None;
    for line in lines {
        let line = line.expect("the child's output");
        println!("{line}");
        if let Some(at) = line.find("done ") {
            done = Some(line[at..].to_string());
        }
    }
    let status = child.wait().expect("the graph process exits");
    assert!(status.success(), "the graph process failed: {status}");
    let done = done.expect("the graph process reported");
    assert!(done.contains("held=0"), "the child's holds: {done}");
    assert!(
        done.contains("copies=[0]"),
        "the child copied frame bytes: {done}"
    );

    assert_eq!(acked, (0..FRAMES).collect::<Vec<_>>());
    let stats = producer.stats();
    assert_eq!(stats.published, FRAMES);
    assert_eq!(
        stats.dropped, 0,
        "no frame was dropped for want of a buffer"
    );
    assert_eq!(copies(), copies_before, "the service copied no frame bytes");
    // With the reader gone its marks are reaped: the service publishes on past the graph's last
    // frame, one buffer after another, and nothing is dropped for want of one.
    for index in 0..(2 * BUFFERS) as u64 {
        let mut lease = producer
            .begin(support::entry(FRAMES + index))
            .expect("a buffer after the graph process exited");
        lease.write().expect("sole owner").bytes().fill(0);
        producer.publish(lease).expect("published");
    }
    assert_eq!(
        producer.stats().dropped,
        0,
        "no frame was dropped after the exit"
    );
    assert_eq!(
        producer.stats().gone,
        1,
        "the graph process's socket was reaped"
    );
    let _ = fs::remove_file(&path);
}

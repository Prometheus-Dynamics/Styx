//! The `styxsrc` element run in a real GStreamer pipeline, against a real producer. Needs the
//! `gstreamer` feature and GStreamer 1.x installed.
//!
//! The pipeline is `styxsrc ! fakesink`: a pad probe on the source pad sees each buffer, and the
//! fake sink drops it right after, so every frame's memory is released as the test goes.

#![cfg(feature = "gstreamer")]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Once};
use std::thread;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_allocators as gst_allocators;
use gstreamer_video as gst_video;
use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, packed_layouts};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const NV12_LEN: usize = (WIDTH * HEIGHT * 3 / 2) as usize;
const JPEG_LEN: usize = 1234;

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        gst::init().expect("GStreamer initialises");
        gststyx::plugin_desc::plugin_register_static().expect("the styx plugin registers");
    });
}

/// What the probe saw of one buffer, sent to the test thread.
#[derive(Debug)]
struct Seen {
    caps: String,
    size: usize,
    memories: u32,
    pts: Option<u64>,
    /// The luma/first bytes: the producer fills each frame with its id, so every byte is the same.
    uniform: bool,
    first_two: [u8; 2],
    last_two: [u8; 2],
    video_meta: Option<(u32, Vec<usize>, Vec<i32>)>,
    /// Whether the buffer's one memory is a dma-buf over the frame's descriptor.
    dmabuf: bool,
}

/// A producer on `name` in the test's temp dir, publishing frames built by `frame` until stopped.
/// Returns the socket path and a stop flag.
fn producer(
    name: &str,
    buffers: usize,
    payload: usize,
    frame: impl Fn(&mut Producer, u64) + Send + 'static,
) -> (PathBuf, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut producer = Producer::new(ProducerConfig {
            payload,
            buffers,
            history: 8,
        })
        .expect("producer");
        producer
            .accept(
                stream,
                Limits {
                    max_held: 2,
                    max_hold: None,
                },
            )
            .expect("handshake");
        let mut id = 1_u64;
        while !stop_flag.load(Ordering::Acquire) {
            frame(&mut producer, id);
            id += 1;
            thread::sleep(Duration::from_millis(3));
        }
    });
    (path, stop, handle)
}

fn nv12_frame(producer: &mut Producer, id: u64) {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    let entry = FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts);
    match producer.begin(entry) {
        Ok(mut lease) => {
            let mut planes = lease
                .write()
                .expect("a new frame's lease is the sole owner");
            planes.bytes().fill(id as u8);
            producer.publish(lease).expect("publish");
        }
        Err(BeginError::NoFreeBuffer) => {}
        Err(err) => panic!("begin: {err}"),
    }
}

fn mjpg_frame(producer: &mut Producer, id: u64) {
    let format = MediaFormat::with_defaults(
        FourCc::MJPG,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = Layouts::new(&[PlaneLayout::new(0, 0, JPEG_LEN)]).expect("layouts");
    let entry = FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts);
    match producer.begin(entry) {
        Ok(mut lease) => {
            let mut planes = lease.write().expect("sole owner");
            let bytes = planes.bytes();
            bytes[..2].copy_from_slice(&[0xFF, 0xD8]);
            bytes[2..JPEG_LEN - 2].fill(0x5A);
            bytes[JPEG_LEN - 2..JPEG_LEN].copy_from_slice(&[0xFF, 0xD9]);
            producer.publish(lease).expect("publish");
        }
        Err(BeginError::NoFreeBuffer) => {}
        Err(err) => panic!("begin: {err}"),
    }
}

/// Runs `styxsrc socket=<path> ! fakesink` until `count` buffers have been seen, and returns them.
fn run(socket: &Path, count: usize) -> Vec<Seen> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("styxsrc")
        .property("socket", socket.to_str().expect("utf-8 path"))
        .build()
        .expect("styxsrc makes");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink makes");
    pipeline.add_many([&src, &sink]).expect("add");
    src.link(&sink).expect("link");

    let (tx, rx) = mpsc::channel::<Seen>();
    let pad = src.static_pad("src").expect("src pad");
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(buffer) = info.buffer() {
            let caps = pad
                .current_caps()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let map = buffer.map_readable().expect("a readable frame");
            let bytes = map.as_slice();
            let uniform = bytes.iter().all(|&b| b == bytes[0]);
            let video_meta = buffer.meta::<gst_video::VideoMeta>().map(|meta| {
                (
                    meta.n_planes(),
                    meta.offset().to_vec(),
                    meta.stride().to_vec(),
                )
            });
            let seen = Seen {
                caps,
                size: bytes.len(),
                memories: buffer.n_memory() as u32,
                pts: buffer.pts().map(|t| t.nseconds()),
                uniform,
                first_two: [bytes[0], bytes[1]],
                last_two: [bytes[bytes.len() - 2], bytes[bytes.len() - 1]],
                video_meta,
                dmabuf: buffer
                    .peek_memory(0)
                    .downcast_memory_ref::<gst_allocators::DmaBufMemory>()
                    .is_some(),
            };
            let _ = tx.send(seen);
        }
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).expect("playing");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while seen.len() < count && Instant::now() < deadline {
        if let Ok(item) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(item);
        }
    }
    pipeline.set_state(gst::State::Null).expect("null");
    seen
}

#[test]
fn the_element_is_built_and_its_socket_property_round_trips() {
    init();
    assert!(gststyx::element_built());
    let src = gst::ElementFactory::make("styxsrc")
        .property("socket", "/tmp/styx-gst-property.sock")
        .build()
        .expect("styxsrc makes");
    assert_eq!(
        src.property::<Option<String>>("socket").as_deref(),
        Some("/tmp/styx-gst-property.sock")
    );
}

#[test]
fn a_frame_reaches_gstreamer_as_nv12_with_the_producers_bytes_and_planes() {
    init();
    let (socket, stop, handle) = producer("element-nv12.sock", 4, NV12_LEN, nv12_frame);
    let seen = run(&socket, 5);
    stop.store(true, Ordering::Release);
    handle.join().expect("producer thread");

    assert!(seen.len() >= 5, "frames arrived: {}", seen.len());
    let mut last_pts = None;
    for frame in &seen {
        assert!(frame.caps.contains("video/x-raw"), "caps: {}", frame.caps);
        assert!(
            frame.caps.contains("format=(string)NV12"),
            "caps: {}",
            frame.caps
        );
        assert!(frame.caps.contains("width=(int)64"), "caps: {}", frame.caps);
        assert!(
            frame.caps.contains("height=(int)32"),
            "caps: {}",
            frame.caps
        );
        assert_eq!(frame.size, NV12_LEN, "the whole frame");
        assert_eq!(frame.memories, 1, "one memory, the frame's own");
        assert!(
            frame.dmabuf,
            "a frame with a descriptor is wrapped as dma-buf memory"
        );
        assert!(frame.uniform, "every byte is the producer's fill");
        let (planes, offsets, strides) = frame.video_meta.clone().expect("a video meta");
        assert_eq!(planes, 2, "NV12 has two planes");
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[1], (WIDTH * HEIGHT) as usize);
        assert_eq!(strides[0], WIDTH as i32);
        assert_eq!(strides[1], WIDTH as i32);
        if let (Some(prev), Some(now)) = (last_pts, frame.pts) {
            assert!(now >= prev, "timestamps do not go back");
        }
        last_pts = frame.pts.or(last_pts);
    }
}

#[test]
fn an_mjpg_frame_reaches_gstreamer_as_image_jpeg_of_its_own_length() {
    init();
    let (socket, stop, handle) = producer("element-mjpg.sock", 4, JPEG_LEN, mjpg_frame);
    let seen = run(&socket, 3);
    stop.store(true, Ordering::Release);
    handle.join().expect("producer thread");

    assert!(seen.len() >= 3, "frames arrived: {}", seen.len());
    for frame in &seen {
        assert!(frame.caps.contains("image/jpeg"), "caps: {}", frame.caps);
        assert!(frame.caps.contains("width=(int)64"), "caps: {}", frame.caps);
        assert_eq!(frame.size, JPEG_LEN, "the frame's own length");
        assert_eq!(frame.first_two, [0xFF, 0xD8], "start marker");
        assert_eq!(frame.last_two, [0xFF, 0xD9], "end marker");
        assert!(
            frame.video_meta.is_none(),
            "compressed frames carry no video meta"
        );
    }
}

#[test]
fn frames_are_given_back_as_gstreamer_frees_them() {
    init();
    // Two buffers in the ring: the producer can only publish again once the element's frames are
    // released. Forty frames through it means every frame came back.
    let (socket, stop, handle) = producer("element-release.sock", 2, NV12_LEN, nv12_frame);
    let seen = run(&socket, 40);
    stop.store(true, Ordering::Release);
    handle.join().expect("producer thread");
    assert!(seen.len() >= 40, "frames arrived: {}", seen.len());
}

#[test]
fn a_source_with_no_socket_does_not_start() {
    init();
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("styxsrc")
        .build()
        .expect("styxsrc makes");
    let sink = gst::ElementFactory::make("fakesink")
        .build()
        .expect("fakesink makes");
    pipeline.add_many([&src, &sink]).expect("add");
    src.link(&sink).expect("link");
    assert!(
        pipeline.set_state(gst::State::Playing).is_err(),
        "no socket, no start"
    );
    let _ = pipeline.set_state(gst::State::Null);
}

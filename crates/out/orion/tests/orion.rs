//! Proof: a reader finds a frame ring by its name on an in-process Orion, and receives the
//! frames through the ring, with no copy.
//!
//! - An in-process comm core (Orion's broker, as its own tests start one) runs in a thread.
//! - A camera thread declares the topic with the ring's advert, and runs a `styx_socket`
//!   producer on the advert's socket.
//! - The test's thread is the reader: it finds the topic by name, connects to the socket the
//!   record names, and reads frames in place. Every frame's bytes are checked, and the copy
//!   counters must read zero at the end.
//! - A name nobody declares is not found.

#![cfg(all(target_os = "linux", feature = "orion"))]

mod common;

use std::num::NonZeroU32;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orion::{Address, Orion};
use styx_format::FourCc;
use styx_frame::copy_counters;
use styx_orion::{Advert, advertise, discover};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

use common::{HEIGHT, PAYLOAD, WIDTH, comm_core, dir, entry};

const TOPIC: &str = "camera.front";
const BUFFERS: u32 = 4;
const FRAMES: u64 = 200;

/// The camera: declares the topic, then serves its frame ring to the one reader that connects.
fn camera(
    orion_socket: PathBuf,
    frames: PathBuf,
    listener: UnixListener,
    advertised: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
    published: Arc<AtomicU64>,
) {
    let orion = Orion::connect(&Address::Local(orion_socket), "camera").expect("camera connects");
    let advert = advert_for(&frames);
    let _topic = advertise(&orion, TOPIC, &advert).expect("advertise");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: BUFFERS as usize,
        history: 8,
    })
    .expect("producer");
    advertised.send(()).expect("the test is waiting");
    let (stream, _) = listener.accept().expect("the reader connects");
    producer
        .accept(
            stream,
            Limits {
                max_held: 2,
                max_hold: None,
            },
        )
        .expect("handshake");
    let mut id = 0_u64;
    while !stop.load(Ordering::Acquire) {
        id += 1;
        match producer.begin(entry(id)) {
            Ok(mut lease) => {
                lease
                    .write()
                    .expect("a new frame's lease is the sole owner")
                    .bytes()
                    .fill(id as u8);
                producer.publish(lease).expect("publish");
                published.fetch_add(1, Ordering::Release);
            }
            // Every buffer is published or held by the reader: the frame is dropped and counted.
            Err(BeginError::NoFreeBuffer) => {}
            Err(err) => panic!("begin: {err}"),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(producer);
    drop(_topic);
}

fn advert_for(frames: &Path) -> Advert {
    Advert {
        code: FourCc::GREY,
        width: NonZeroU32::new(WIDTH).expect("width"),
        height: NonZeroU32::new(HEIGHT).expect("height"),
        payload: PAYLOAD as u32,
        buffers: BUFFERS,
        socket: frames.to_string_lossy().into_owned(),
    }
}

#[test]
fn a_reader_finds_the_ring_by_name_and_reads_its_frames_with_no_copy() {
    let dir = dir("discover");
    let orion_socket = dir.join("orion.sock");
    let frames = dir.join("frames.sock");

    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");

    let listener = UnixListener::bind(&frames).expect("bind");
    let (advertised, advertising) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let published = Arc::new(AtomicU64::new(0));
    let camera_thread = {
        let (orion_socket, frames) = (orion_socket.clone(), frames.clone());
        let (stop, published) = (stop.clone(), published.clone());
        std::thread::spawn(move || {
            camera(orion_socket, frames, listener, advertised, stop, published);
        })
    };
    advertising.recv().expect("the camera advertises");

    // Nothing is declared under this name: the lookup gives up and says so.
    let reader_orion =
        Orion::connect(&Address::Local(orion_socket.clone()), "reader").expect("reader connects");
    let nothing = discover(&reader_orion, "nobody.declared", Duration::from_millis(300))
        .expect("the lookup runs");
    assert!(nothing.is_none(), "a name nobody declared was found");

    // The reader finds the ring by its name.
    let found = discover(&reader_orion, TOPIC, Duration::from_secs(20))
        .expect("discover")
        .expect("the topic is found by name");
    assert_eq!(
        found.advert,
        advert_for(&frames),
        "the record is what the camera wrote"
    );

    // It connects to the ring the record names, and reads the frames in place.
    let mut reader = found.connect().expect("the producer's handshake");
    let started = Instant::now();
    let mut seen = 0_u64;
    let mut last_id = 0_u64;
    while seen < FRAMES {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "saw {seen} of {FRAMES} frames"
        );
        match reader.next_frame().expect("not cut off") {
            Some(frame) => {
                let id = frame.lease().id();
                assert!(id > last_id, "frames arrive in order: {id} after {last_id}");
                last_id = id;
                let read = frame.lease().read().expect("mapped for the CPU");
                let plane = read.plane(0).expect("one plane");
                assert_eq!(plane.data.len(), PAYLOAD);
                assert!(
                    plane.data.iter().all(|byte| *byte == id as u8),
                    "frame {id} is not what the camera wrote"
                );
                seen += 1;
            }
            None => {
                let _ = reader.wait(Some(Duration::from_millis(50)));
            }
        }
    }
    let stats = reader.stats();

    stop.store(true, Ordering::Release);
    camera_thread.join().expect("the camera ends");
    let _ = stop_comm.send(());

    let copies: Vec<_> = copy_counters()
        .iter()
        .map(|counter| (counter.site(), counter.copies()))
        .collect();
    assert!(
        copy_counters().iter().all(|counter| counter.copies() == 0),
        "copies: {copies:?}"
    );
    println!(
        "orion: found {TOPIC} by name; {seen} frames read in place (delivered {}, skipped {}); \
         published {}; copies {copies:?}",
        stats.delivered,
        stats.skipped,
        published.load(Ordering::Acquire),
    );
}

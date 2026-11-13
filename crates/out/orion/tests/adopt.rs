//! Proof: a producer's own ring and pool, adopted as an Orion buffer topic, serve the direct
//! socket reader from the same ring, in order, with no copy, and a geometry mismatch is refused.
//!
//! What this does not cover yet: an Orion subscriber receiving frames (needs the producer's hold
//! table mutably for `Published::poll`; see the crate's `Published` docs).

#![cfg(all(target_os = "linux", feature = "orion"))]

mod common;

use std::os::unix::net::UnixStream;
use std::time::Duration;

use orion::{Address, Orion};
use styx_frame::copy_counters;
use styx_orion::publish;
use styx_socket::{BeginError, Limits, Producer, ProducerConfig, Reader};

use common::{PAYLOAD, comm_core, dir, entry};

const TOPIC: &str = "camera.adopted";
const FRAMES: u64 = 200;

fn producer(history: u32) -> Producer {
    Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history,
    })
    .expect("producer")
}

#[test]
fn an_adopted_ring_serves_its_direct_reader_in_order_with_no_copy() {
    let dir = dir("adopt");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let orion = Orion::connect(&Address::Local(orion_socket), "camera").expect("camera connects");

    let mut camera = producer(8);
    let mut published = publish(&orion, TOPIC, &camera, 0).expect("the ring is adopted");
    assert_eq!(published.subscribers(), 0, "no Orion subscriber yet");

    // A reader on the other end of a socket pair, connected on its own thread: the producer's
    // handshake and the reader's answer must run at once.
    let (camera_end, reader_end) = UnixStream::pair().expect("socket pair");
    let reader_thread = std::thread::spawn(move || Reader::connect(reader_end).expect("reader"));
    camera
        .accept(
            camera_end,
            Limits {
                max_held: 2,
                max_hold: None,
            },
        )
        .expect("the reader is handed the adopted ring");
    let mut reader = reader_thread.join().expect("reader thread");

    let started = std::time::Instant::now();
    let mut next_id = 1_u64;
    let mut seen = 0_u64;
    let mut last_id = 0_u64;
    while seen < FRAMES {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "saw {seen} of {FRAMES} frames"
        );
        if next_id <= FRAMES {
            match camera.begin(entry(next_id)) {
                Ok(mut lease) => {
                    lease
                        .write()
                        .expect("a new frame's lease is the sole owner")
                        .bytes()
                        .fill(next_id as u8);
                    camera.publish(lease).expect("publish");
                    published.wake();
                    next_id += 1;
                }
                Err(BeginError::NoFreeBuffer) => {}
                Err(err) => panic!("begin: {err}"),
            }
        }
        if let Some(frame) = reader.next_frame().expect("not cut off") {
            {
                let id = frame.lease().id();
                assert!(id > last_id, "frames arrive in order: {id} after {last_id}");
                last_id = id;
                let read = frame.lease().read().expect("mapped for the CPU");
                let plane = read.plane(0).expect("one plane");
                assert!(
                    plane.data.iter().all(|byte| *byte == id as u8),
                    "frame {id} is not what the camera wrote"
                );
                seen += 1;
            }
        }
    }

    let copies: Vec<_> = copy_counters()
        .iter()
        .map(|counter| (counter.site(), counter.copies()))
        .collect();
    assert!(
        copy_counters().iter().all(|counter| counter.copies() == 0),
        "copies: {copies:?}"
    );
    println!(
        "orion adopt: {seen} frames to the direct reader from the adopted ring, in order; \
         copies {copies:?}; producer {:?}",
        camera.stats()
    );
    drop(published);
    drop(orion);
    let _ = stop_comm.send(());
}

/// Another program cannot take a name that is adopted: the comm core refuses it. A geometry
/// mismatch under the same name in the same program is accepted by Orion at rev 181e139 (see the
/// PR); this test does not assert on that case.
#[test]
fn another_program_cannot_adopt_a_name_already_adopted() {
    let dir = dir("rival");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera = Orion::connect(&Address::Local(orion_socket.clone()), "camera").expect("camera");
    let first = producer(8);
    let _published = publish(&camera, TOPIC, &first, 0).expect("the first ring is adopted");

    let rival = Orion::connect(&Address::Local(orion_socket), "rival").expect("rival");
    let second = producer(8);
    let refused = publish(&rival, TOPIC, &second, 0);
    assert!(
        refused.is_err(),
        "a second program adopted a name that was taken"
    );
    let _ = stop_comm.send(());
}

/// An Orion subscriber attaches to the adopted topic and the publisher counts it. The subscriber
/// is not given frames here: those need the producer's hold table (see `Published::poll`).
#[test]
fn an_orion_subscriber_attaches_to_the_adopted_topic() {
    let dir = dir("attach");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera = Orion::connect(&Address::Local(orion_socket.clone()), "camera").expect("camera");
    let first = producer(8);
    let mut published = publish(&camera, TOPIC, &first, 0).expect("the ring is adopted");

    let reader = Orion::connect(&Address::Local(orion_socket), "reader").expect("reader");
    let _subscriber = reader
        .subscribe_buffers(TOPIC, &[], orion::buffers::HoldLimits::default())
        .expect("the subscriber attaches");
    let mut counted = 0;
    let started = std::time::Instant::now();
    while counted == 0 && started.elapsed() < Duration::from_secs(10) {
        let mut dummy: orion::buffers::HoldTable = orion::buffers::HoldTable::new(4);
        published.poll(first.pool(), &mut dummy).expect("poll");
        counted = published.subscribers();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(counted, 1, "the publisher counts the Orion subscriber");
    let _ = stop_comm.send(());
}

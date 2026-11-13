//! Proof: a producer's own ring and pool, adopted as an Orion buffer topic, serve an Orion
//! subscriber and a direct socket reader from the same ring, in order, bytes equal, with no copy.
//! The subscriber wakes on its doorbell (`Published::wake`), not on a timer. A ring whose shape is
//! not its declaration is refused as malformed, and a withdrawal is seen by the subscriber.

#![cfg(all(target_os = "linux", feature = "orion"))]

mod common;

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use lemnos_linux_sys::dma_heap;
use orion::topics::Status;
use orion::{Address, Orion};
use shm_ring_buffers::entry;
use std::os::fd::AsFd;
use styx_frame::{BufferMemory, BufferSize, FdMemory, FramePool, copy_counters};
use styx_orion::{Published, Subscription, publish, subscribe};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig, Reader};

use common::{PAYLOAD, comm_core, dir, entry as frame_entry};

const TOPIC: &str = "camera.adopted";
const FRAMES: u64 = 200;
const BUFFERS: usize = 4;
/// GPU slack after each frame so a dma-buf buffer is a page.
const SLACK: usize = 4096 - PAYLOAD;
/// One buffer: payload and slack.
const BUFFER: usize = PAYLOAD + SLACK;
/// Orion may give its subscribers slots 32 and up; direct readers take the lowest free slot.
const ORION_SLOTS: u64 = 0xFFFF_FFFF_0000_0000;

fn producer(history: u32) -> Producer {
    Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: BUFFERS,
        history,
    })
    .expect("producer")
}

/// Connects a direct socket reader to `camera`, handing it the adopted ring.
fn direct_reader(camera: &mut Producer) -> Reader {
    let (camera_end, reader_end) = UnixStream::pair().expect("socket pair");
    let thread = std::thread::spawn(move || Reader::connect(reader_end).expect("reader"));
    camera
        .accept(
            camera_end,
            Limits {
                max_held: 2,
                max_hold: None,
            },
        )
        .expect("the reader is handed the adopted ring");
    thread.join().expect("reader thread")
}

/// Pumps the publisher and the subscriber until the subscriber is in the hold table and has been
/// offered the pool. The offer reaches the subscriber's session, so the subscriber must be pumped
/// (its `wait`) as well as the publisher.
fn await_pool(published: &mut Published, camera: &mut Producer, sub: &mut Subscription) {
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the subscriber was never offered the pool"
        );
        published.poll(camera).expect("poll");
        let _ = sub.wait(Duration::from_millis(5));
        if published.subscribers() == 1 && sub.pool_buffers() == Some(BUFFERS) {
            return;
        }
    }
}

/// Publishes frame `id` into `camera`, and rings Orion's subscribers.
fn publish_frame(camera: &mut Producer, published: &mut Published, id: u64) -> bool {
    match camera.begin(frame_entry(id)) {
        Ok(mut lease) => {
            lease
                .write()
                .expect("a new frame's lease is the sole owner")
                .bytes()
                .fill(id as u8);
            camera.publish(lease).expect("publish");
            published.wake();
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

/// Reads what is ready from a direct reader, as (id, bytes).
fn drain_direct(reader: &mut Reader, out: &mut Vec<(u64, Vec<u8>)>) {
    while let Some(frame) = reader.next_frame().expect("not cut off") {
        let id = frame.lease().id();
        let read = frame.lease().read().expect("mapped for the CPU");
        out.push((id, read.plane(0).expect("one plane").data.to_vec()));
    }
}

/// Reads what is ready from an Orion subscriber, as (id, bytes).
fn drain_orion(sub: &mut Subscription, out: &mut Vec<(u64, Vec<u8>)>) {
    while let Some(frame) = sub.next_frame().expect("the frames read") {
        let id = frame.lease().id();
        let read = frame.lease().read().expect("mapped for the CPU");
        out.push((id, read.plane(0).expect("one plane").data.to_vec()));
    }
}

fn check_no_copies() -> Vec<(&'static str, usize)> {
    let copies: Vec<_> = copy_counters()
        .iter()
        .map(|counter| (counter.site(), counter.copies()))
        .collect();
    assert!(
        copy_counters().iter().all(|counter| counter.copies() == 0),
        "copies: {copies:?}"
    );
    copies
}

#[test]
fn an_orion_subscriber_and_a_direct_reader_get_the_same_frames_from_one_ring() {
    both_readers_get_the_same_frames(producer(8), "memfd");
}

/// The same proof over a dma-buf pool: the pool is a dma-heap buffer, handed to the producer with
/// `Producer::with_pool`. Skipped, with the reason printed, where there is no dma-heap.
#[test]
fn an_orion_subscriber_and_a_direct_reader_get_the_same_frames_from_a_dmabuf_pool() {
    let Some(heap) = dma_heap() else {
        return;
    };
    let len = BUFFERS * BUFFER;
    let Ok(dmabuf) = dma_heap::allocate(heap.as_fd(), len as u64) else {
        println!("skipped: the dma-heap would not allocate {len} bytes");
        return;
    };
    let memories: Vec<Box<dyn BufferMemory>> = (0..BUFFERS)
        .map(|index| {
            let fd = dmabuf.try_clone().expect("a descriptor of the dma-buf");
            Box::new(FdMemory::dmabuf_at(fd, index * BUFFER, BUFFER, true).expect("mapped"))
                as Box<dyn BufferMemory>
        })
        .collect();
    let pool: FramePool<()> =
        FramePool::new(BufferSize::new(PAYLOAD, SLACK), memories).expect("the pool");
    let camera = Producer::with_pool(pool, 8).expect("a producer over the dma-buf pool");
    both_readers_get_the_same_frames(camera, "dmabuf");
}

fn dma_heap() -> Option<std::fs::File> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dma_heap/system")
    {
        Ok(heap) => Some(heap),
        Err(err) => {
            println!("skipped: no dma-heap here ({err})");
            None
        }
    }
}

fn both_readers_get_the_same_frames(mut camera: Producer, memory: &str) {
    let dir = dir(memory);
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera_orion =
        Orion::connect(&Address::Local(orion_socket.clone()), "camera").expect("camera");
    let viewer = Orion::connect(&Address::Local(orion_socket), "viewer").expect("viewer");

    let mut published =
        publish(&camera_orion, TOPIC, &camera, ORION_SLOTS).expect("the ring is adopted");
    let mut direct = direct_reader(&mut camera);
    let mut orion_sub = subscribe(&viewer, TOPIC, 2).expect("the subscriber attaches");
    await_pool(&mut published, &mut camera, &mut orion_sub);
    assert_eq!(
        orion_sub.pool_buffers(),
        Some(BUFFERS),
        "the offered pool, as offered"
    );
    assert_eq!(
        camera.holds().readers(),
        2,
        "the direct reader and the subscriber hold"
    );

    let mut from_direct = Vec::new();
    let mut from_orion = Vec::new();
    let mut woke = 0_u64;
    let mut next_id = 1_u64;
    let started = Instant::now();
    while (from_direct.len() as u64) < FRAMES || (from_orion.len() as u64) < FRAMES {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "direct {} orion {}",
            from_direct.len(),
            from_orion.len()
        );
        published.poll(&mut camera).expect("poll");
        if next_id <= FRAMES && publish_frame(&mut camera, &mut published, next_id) {
            // The subscriber sleeps on its doorbell: its wait returns true for the frame just
            // published, not on a timeout.
            let before = Instant::now();
            let ready = orion_sub.wait(Duration::from_secs(2)).expect("wait");
            assert!(ready, "the subscriber was not woken for frame {next_id}");
            assert!(
                before.elapsed() < Duration::from_secs(1),
                "the wake took too long"
            );
            woke += 1;
            next_id += 1;
        }
        drain_direct(&mut direct, &mut from_direct);
        drain_orion(&mut orion_sub, &mut from_orion);
    }

    let expected: Vec<(u64, Vec<u8>)> = (1..=FRAMES)
        .map(|id| (id, vec![id as u8; PAYLOAD]))
        .collect();
    assert_eq!(
        from_direct, expected,
        "the direct reader's frames: in order, as written"
    );
    assert_eq!(
        from_orion, expected,
        "the subscriber's frames: in order, as written"
    );
    let copies = check_no_copies();
    println!(
        "orion both ({memory}): {} frames to the Orion subscriber and to the direct reader from one ring, in order, bytes equal; \
         subscriber woken {woke} times on its doorbell; copies {copies:?}; producer {:?}",
        from_orion.len(),
        camera.stats()
    );
    drop(orion_sub);
    drop(direct);
    drop(published);
    let _ = stop_comm.send(());
}

#[test]
fn a_ring_of_another_shape_than_its_declaration_is_refused_as_malformed() {
    let dir = dir("malformed");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera = Orion::connect(&Address::Local(orion_socket), "camera").expect("camera");

    let first = producer(8);
    let _published =
        publish(&camera, TOPIC, &first, ORION_SLOTS).expect("the first ring is adopted");
    publish(&camera, TOPIC, &first, ORION_SLOTS).expect("the same memory again resumes");

    // The same name, the first ring, but a declaration of another history than the ring's.
    let meta = entry::len(1, first.geometry().meta_max()) as u32 - 72;
    let refused = camera.adopt_buffers(TOPIC, 4, meta, first.ring(), first.pool(), 0);
    let text = refused
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    println!("orion ring of another shape refused: {text}");
    assert!(
        refused.is_err(),
        "a ring of another shape than its declaration was adopted"
    );
    assert!(
        text.contains("malformed"),
        "not refused as malformed: {text}"
    );

    // The same declaration with another ring of the same shape is checked, and accepted.
    let other = producer(8);
    publish(&camera, TOPIC, &other, ORION_SLOTS)
        .expect("another ring of the declared shape is accepted");
    let _ = stop_comm.send(());
}

#[test]
fn a_withdrawal_is_seen_by_the_subscriber_and_the_producer_goes_on() {
    let dir = dir("withdraw");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera_orion =
        Orion::connect(&Address::Local(orion_socket.clone()), "camera").expect("camera");
    let viewer = Orion::connect(&Address::Local(orion_socket), "viewer").expect("viewer");

    let mut camera = producer(8);
    let mut published =
        publish(&camera_orion, TOPIC, &camera, ORION_SLOTS).expect("the ring is adopted");
    let mut direct = direct_reader(&mut camera);
    let mut orion_sub = subscribe(&viewer, TOPIC, 2).expect("the subscriber attaches");
    await_pool(&mut published, &mut camera, &mut orion_sub);
    assert_eq!(
        orion_sub.status(),
        Status::Live,
        "live before the withdrawal"
    );

    published.withdraw().expect("withdraw");

    let started = Instant::now();
    while orion_sub.status() != Status::PublisherGone {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the subscriber was not told"
        );
        let _ = orion_sub.wait(Duration::from_millis(100));
    }

    // The producer's own reader is not affected: a frame published now still reaches it.
    let mut lease = camera
        .begin(frame_entry(1))
        .expect("the producer still takes a buffer");
    lease.write().expect("sole owner").bytes().fill(1);
    camera.publish(lease).expect("the producer still publishes");
    let mut seen = Vec::new();
    let started = Instant::now();
    while seen.is_empty() && started.elapsed() < Duration::from_secs(5) {
        drain_direct(&mut direct, &mut seen);
    }
    assert_eq!(
        seen.len(),
        1,
        "the direct reader stopped after the withdrawal"
    );
    println!(
        "orion withdraw: subscriber status {:?}; direct reader read {} frame after it",
        orion_sub.status(),
        seen.len()
    );
    let _ = stop_comm.send(());
}

#[test]
fn another_program_cannot_adopt_a_name_already_adopted() {
    let dir = dir("rival");
    let orion_socket = dir.join("orion.sock");
    let (started, stop_comm) = comm_core(orion_socket.clone());
    started.recv().expect("the comm core starts");
    let camera = Orion::connect(&Address::Local(orion_socket.clone()), "camera").expect("camera");
    let first = producer(8);
    let _published =
        publish(&camera, TOPIC, &first, ORION_SLOTS).expect("the first ring is adopted");

    let rival = Orion::connect(&Address::Local(orion_socket), "rival").expect("rival");
    let second = producer(8);
    let refused = publish(&rival, TOPIC, &second, ORION_SLOTS);
    assert!(
        refused.is_err(),
        "a second program adopted a name that was taken"
    );
    let _ = stop_comm.send(());
}

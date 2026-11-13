//! Frames from the shared ring as `styx:frame` values. The producer and the reader are a real
//! pair over a Unix socket; the value holds the reader's ring hold until it drops.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

mod support;

use std::os::unix::net::UnixStream;
use std::thread;

use daedalus_dmabuf::DmaBufBacked;
use styx_daedalus::{StyxFrame, latest_value, next_value};
use styx_frame::MemoryKind;
use styx_socket::{Limits, Producer, ProducerConfig, Reader};

fn connected() -> (Producer, Reader) {
    let mut producer = Producer::new(ProducerConfig {
        payload: 4096,
        buffers: 3,
        history: 8,
    })
    .expect("a producer");
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let reader = thread::spawn(move || Reader::connect(theirs).expect("a reader"));
    producer.accept(ours, Limits::default()).expect("accepted");
    (producer, reader.join().expect("the reader thread"))
}

fn publish(producer: &mut Producer, id: u64) {
    let mut lease = producer.begin(support::entry(id)).expect("a free buffer");
    lease.write().expect("sole owner").bytes().fill(id as u8);
    producer.publish(lease).expect("published");
}

#[test]
fn a_ring_frame_is_a_styx_frame_value_that_holds_its_buffer_until_it_drops() {
    let (mut producer, mut reader) = connected();
    publish(&mut producer, 11);
    let value: StyxFrame = next_value(&mut reader).unwrap().expect("a frame");
    assert_eq!(value.id(), 11);
    assert_eq!(value.timestamp_ns(), 1_011);
    assert_eq!(value.memory_kind(), MemoryKind::Memfd);
    assert!(
        value.dmabuf().is_none(),
        "a ring frame is memfd, not a dma-buf"
    );
    assert_eq!(reader.stats().held, 1, "the value holds the ring's buffer");

    drop(value);
    assert_eq!(
        reader.stats().held,
        0,
        "the last value went: the hold is released"
    );
}

#[test]
fn the_latest_ring_frame_is_one_value_and_the_older_ones_are_passed_over() {
    let (mut producer, mut reader) = connected();
    for id in 1..=3 {
        publish(&mut producer, id);
    }
    let value = latest_value(&mut reader).unwrap().expect("a frame");
    assert_eq!(value.id(), 3);
    assert_eq!(reader.stats().superseded, 2);
    drop(value);
    assert_eq!(reader.stats().held, 0);
}

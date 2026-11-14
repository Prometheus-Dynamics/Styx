//! The handshake's version, both ways, through the real handshake code. One side is this build;
//! the other sends the old version's constant in its first word, as a version 4 build does.
//! Each refusal is a typed error naming both versions, and nothing is linked.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shm_fdpass as fdpass;
use styx_socket::{
    HANDSHAKE_VERSION, Limits, Producer, ProducerConfig, Reader, SharedLayout, SocketError, Start,
};

/// The version an old build sent. Version 4 is the last one before this one.
const OLD: u32 = 4;
/// An offer's length: the version, the geometry and counts, and the descriptor count.
const OFFER_LEN: usize = 4 * 6 + 8 * 2 + 4;
/// A buffer record's length.
const RECORD_LEN: usize = 28;
/// A reader's answer: its version and its slot.
const ACK_LEN: usize = 8;

fn fd() -> OwnedFd {
    OwnedFd::from(File::open("/dev/null").expect("/dev/null"))
}

#[test]
fn a_new_reader_refuses_an_old_offer_and_names_both_versions() {
    assert_eq!(
        HANDSHAKE_VERSION, 5,
        "this build's version is the one the tests pin"
    );
    let (publisher, reader_side) = UnixStream::pair().expect("socket pair");
    // The old publisher's offer: its first word is the old version. The two descriptors are the
    // ring and the pool page, which the version is refused before reading.
    let mut offer = [0_u8; OFFER_LEN];
    offer[0..4].copy_from_slice(&OLD.to_le_bytes());
    let (ring, page) = (fd(), fd());
    fdpass::send_on(&publisher, &[&offer], &[ring.as_fd(), page.as_fd()]).expect("send offer");

    let err = Reader::connect_with(reader_side, Start::NextNew).expect_err("refused");
    let text = err.to_string();
    match err {
        SocketError::Version { ours, theirs } => {
            assert_eq!(ours, HANDSHAKE_VERSION);
            assert_eq!(theirs, OLD);
        }
        other => panic!("expected the version refusal, got: {other}"),
    }
    assert!(
        text.contains(&format!("version {OLD}"))
            && text.contains(&format!("version {HANDSHAKE_VERSION}")),
        "{text}"
    );
}

#[test]
fn a_new_producer_refuses_an_old_answer_names_both_versions_and_links_nothing() {
    let producer = Producer::<()>::new(ProducerConfig {
        payload: 64,
        buffers: 2,
        history: 4,
    })
    .expect("producer");
    let plan = producer.handshake_plan().expect("plan");
    let (ours, old_reader) = UnixStream::pair().expect("socket pair");

    // The old reader: takes the offer and the two records, then answers with the old version.
    let old = std::thread::spawn(move || {
        let mut offer = [0_u8; OFFER_LEN];
        // The offer carries three descriptors: the ring, the pool page and the state page.
        fdpass::recv(old_reader.as_fd(), &mut offer, 3, false)
            .expect("offer")
            .expect("the producer sent an offer");
        for _ in 0..2 {
            let mut record = [0_u8; RECORD_LEN];
            fdpass::recv(old_reader.as_fd(), &mut record, 1, false)
                .expect("record")
                .expect("the producer sent a record");
        }
        let mut ack = [0_u8; ACK_LEN];
        ack[0..4].copy_from_slice(&OLD.to_le_bytes());
        let (a, b, c) = (fd(), fd(), fd());
        fdpass::send_on(&old_reader, &[&ack], &[a.as_fd(), b.as_fd(), c.as_fd()]).expect("answer");
        old_reader
    });

    let err = plan
        .run(ours, Limits::default(), Duration::from_secs(5))
        .expect_err("the old answer is refused");
    let _old_reader = old.join().expect("old reader thread");
    let text = err.to_string();
    match err {
        SocketError::Version { ours, theirs } => {
            assert_eq!(ours, HANDSHAKE_VERSION);
            assert_eq!(theirs, OLD);
        }
        other => panic!("expected the version refusal, got: {other}"),
    }
    assert!(
        text.contains(&format!("version {OLD}"))
            && text.contains(&format!("version {HANDSHAKE_VERSION}")),
        "{text}"
    );
    // The plan's run is the only step that could link the reader; it failed, so nothing did.
    assert_eq!(
        producer.stats().readers,
        0,
        "the refused reader is not linked"
    );
}

#[test]
fn a_shared_layout_refusal_keeps_its_structure_and_both_values() {
    for (what, which) in [
        ("this ring", SharedLayout::Ring),
        ("this marks page", SharedLayout::Marks),
    ] {
        let err = SocketError::from(shm_ring::RingError::Layout {
            what,
            found: 1,
            reads: 2,
        });
        match &err {
            SocketError::Layout {
                which: got,
                found,
                expected,
            } => {
                assert_eq!((*got, *found, *expected), (which, 1, 2));
            }
            other => panic!("expected the layout refusal, got: {other}"),
        }
        let text = err.to_string();
        assert!(
            text.contains("layout 1") && text.contains("layout 2"),
            "{text}"
        );
        assert!(text.contains(which.what()), "{text}");
    }
}

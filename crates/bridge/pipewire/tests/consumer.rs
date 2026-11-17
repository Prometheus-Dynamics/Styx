//! Frames reach a PipeWire consumer, and each is the frame the producer wrote: a producer whose
//! four buffers are one memfd cut at offsets, the node, and GStreamer's `pipewiresrc` reading the
//! node by name into a file. Every frame's luma is its id, so a consumer that mapped the wrong
//! part of the memfd (every buffer at offset 0, say) reads the same id several times over.
//!
//! The file sink writes each buffer as it comes (`buffer-mode=unbuffered`). Left at its default it
//! keeps every buffer it is handed until 64 KiB have gathered, and the node's ring is four
//! buffers: with frames under 16 KiB (64x48 is 4608 bytes) it holds all four and no frame can
//! follow. That is the consumer's hold, not the node's, so the proof runs from the smallest NV12
//! frame (2x2, six bytes) up.
//!
//! The same proof covers the node's hold: a frame whose luma is two ids is one the producer
//! rewrote while the consumer read it, which is what happened when the node took PipeWire's
//! return of a buffer as the consumer's (PipeWire returns it a cycle after sending, taken or not).
//!
//! Needs this build's PipeWire node, a running PipeWire daemon, and `gst-launch-1.0` with
//! `pipewiresrc`. Without one of them the test prints `SKIPPED` and why.

#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rig, has_pipewiresrc, no_node_or_daemon, one_at_a_time, refuse_a_required_skip};

const FRAMES: usize = 40;

/// Why this test cannot run here, if it cannot.
fn missing() -> Option<String> {
    no_node_or_daemon().or_else(|| {
        refuse_a_required_skip(
            (!has_pipewiresrc()).then(|| "no gst-inspect-1.0 with a pipewiresrc element".into()),
        )
    })
}

#[test]
fn a_pipewire_consumer_reads_each_frame_the_producer_wrote() {
    proof(160, 120);
}

/// The smallest NV12 frame there is: four buffers of six bytes, all inside one page.
#[test]
fn the_smallest_frame_reaches_a_pipewire_consumer() {
    proof(2, 2);
}

/// A size whose four buffers together stay under the file sink's 64 KiB default hold.
#[test]
fn a_small_frame_reaches_a_pipewire_consumer() {
    proof(64, 48);
}

fn proof(width: u32, height: u32) {
    if let Some(reason) = missing() {
        println!("SKIPPED: {reason}");
        return;
    }
    let luma = (width * height) as usize;
    let payload = luma * 3 / 2;
    let name = format!("styx-consumer-proof-{width}x{height}");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let out = dir.join(format!("pw-{width}x{height}.raw"));
    let _ = fs::remove_file(&out);
    let _turn = one_at_a_time();
    let rig = Rig::start(&name, width, height, Duration::from_millis(33), 4);
    let control = rig.control.clone();

    // The consumer: `FRAMES` buffers from the node, by its name, written as they are.
    let caps = format!("video/x-raw,format=NV12,width={width},height={height}");
    let mut consumer = Command::new("gst-launch-1.0")
        .args(["-q", "pipewiresrc"])
        .arg(format!("target-object={name}"))
        .arg(format!("num-buffers={FRAMES}"))
        .args(["!", &caps, "!", "filesink", "buffer-mode=unbuffered"])
        .arg(format!("location={}", out.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gst-launch-1.0");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = consumer.try_wait().expect("poll the consumer") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = consumer.kill();
            let said = consumer
                .wait_with_output()
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_default();
            panic!(
                "the consumer got no {FRAMES} frames: {:?}\nconsumer: {said}",
                control.status()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stats = rig.finish();
    assert!(status.success(), "the consumer failed: {status:?}");

    let bytes = fs::read(&out).expect("the consumer's frames");
    let _ = fs::remove_file(&out);
    assert_eq!(bytes.len(), FRAMES * payload, "whole frames");
    let ids: Vec<u8> = bytes
        .chunks_exact(payload)
        .map(|frame| {
            let id = frame[0];
            if let Some(at) = frame[..luma].iter().position(|&b| b != id) {
                let end = frame[..luma].iter().rposition(|&b| b != frame[at]);
                panic!(
                    "a frame whose luma is not one id: {id} up to byte {at}, then {} (last byte \
                     of another value: {end:?}) of {luma}",
                    frame[at]
                );
            }
            assert!(
                frame[luma..].iter().all(|&b| b == 0x80),
                "a frame whose chroma is not the producer's"
            );
            id
        })
        .collect();
    // Each buffer is a newer frame than the one before it: ids rise (modulo the byte), by however
    // many the consumer was slow for. A stale buffer would repeat an id or step back.
    for pair in ids.windows(2) {
        let step = pair[1].wrapping_sub(pair[0]);
        assert!(
            (1..=127).contains(&step),
            "frame ids {ids:?}: {} then {}",
            pair[0],
            pair[1]
        );
    }
    assert!(stats.frames as usize >= FRAMES, "{stats:?}");
    println!(
        "proof: a PipeWire consumer read {FRAMES} {width}x{height} frames, ids {} to {}, each the producer's; node {stats:?}",
        ids[0],
        ids[FRAMES - 1]
    );
}

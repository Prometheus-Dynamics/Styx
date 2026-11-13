//! A Styx camera stand-in for the bridge proofs: publishes NV12 frames over the shared ring on a
//! Unix socket, so `styxsrc` and the PipeWire node have something to read.
//!
//! Usage: `styx_producer <socket> <fps> <seconds>`. Readers may connect at any time while it runs.
//! Each frame's luma is filled with its id (low byte), so a consumer can check what it received.

use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const BUFFERS: usize = 4;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, socket, fps, seconds] = args.as_slice() else {
        eprintln!("usage: styx_producer <socket> <fps> <seconds>");
        std::process::exit(2);
    };
    let fps: u64 = fps.parse().expect("fps");
    let seconds: u64 = seconds.parse().expect("seconds");
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    let payload = layouts.span().expect("span");
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let mut producer = Producer::new(ProducerConfig {
        payload,
        buffers: BUFFERS,
        history: 8,
    })
    .expect("producer");

    let period = Duration::from_micros(1_000_000 / fps.max(1));
    let started = Instant::now();
    let mut next = started;
    let mut id = 1_u64;
    let mut readers = 0_u32;
    println!("styx_producer: {socket} NV12 {WIDTH}x{HEIGHT} {fps} fps, {BUFFERS} buffers");
    while started.elapsed() < Duration::from_secs(seconds) {
        while let Ok((stream, _)) = listener.accept() {
            stream
                .set_nonblocking(false)
                .expect("blocking reader socket");
            match producer.accept(
                stream,
                Limits {
                    max_held: 2,
                    max_hold: None,
                },
            ) {
                Ok(slot) => {
                    readers += 1;
                    println!("styx_producer: reader {readers} on slot {slot}");
                }
                Err(err) => eprintln!("styx_producer: handshake failed: {err}"),
            }
        }
        let entry = FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts);
        match producer.begin(entry) {
            Ok(mut lease) => {
                let mut planes = lease.write().expect("sole owner of a new frame");
                let bytes = planes.bytes();
                let luma = (WIDTH * HEIGHT) as usize;
                bytes[..luma].fill(id as u8);
                bytes[luma..].fill(0x80);
                producer.publish(lease).expect("publish");
            }
            Err(BeginError::NoFreeBuffer) => {}
            Err(err) => panic!("begin: {err}"),
        }
        id += 1;
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    let stats = producer.stats();
    println!(
        "styx_producer: published {} dropped {} readers {} cut_off {}",
        stats.published, stats.dropped, stats.readers, stats.cut_off
    );
}

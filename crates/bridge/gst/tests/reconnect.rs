//! `styxsrc`'s Rust half over a real producer that goes away mid-stream and comes back on the same
//! socket: the same [`Source`] keeps running, backs off while the producer is away (its status
//! says so, with the delay until the next attempt), and delivers the new producer's frames when it
//! returns. Needs no GStreamer and no PipeWire.

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_gst::{LinkState, Source};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT * 3 / 2) as usize;

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

fn publish(producer: &mut Producer, id: u64) {
    match producer.begin(entry(id)) {
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
}

/// A producer on `socket`, publishing until stopped. Stopping drops it, so its readers' sockets
/// close: that is what a producer going away looks like to a reader.
struct Going {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

fn start(socket: &Path) -> Going {
    let socket: PathBuf = socket.to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = thread::spawn(move || {
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let mut producer = Producer::new(ProducerConfig {
            payload: PAYLOAD,
            buffers: 4,
            history: 8,
        })
        .expect("producer");
        let mut id = 1_u64;
        while !flag.load(Ordering::Acquire) {
            while let Ok((stream, _)) = listener.accept() {
                stream.set_nonblocking(false).expect("blocking");
                let _ = producer.accept(
                    stream,
                    Limits {
                        max_held: 2,
                        max_hold: None,
                    },
                );
            }
            publish(&mut producer, id);
            id += 1;
            thread::sleep(Duration::from_millis(3));
        }
        drop(producer);
        drop(listener);
        let _ = fs::remove_file(&socket);
    });
    Going { stop, thread }
}

impl Going {
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("producer thread");
    }
}

/// Pulls frames from `source` for `for_` and returns their ids.
fn pull(source: &mut Source, for_: Duration) -> Vec<u64> {
    let until = Instant::now() + for_;
    let mut ids = Vec::new();
    while Instant::now() < until {
        if let Some(frame) = source.next(Duration::from_millis(5)) {
            ids.push(frame.id());
        }
    }
    ids
}

#[test]
fn the_source_keeps_running_across_its_producer_going_away_and_coming_back() {
    let socket = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gst-reconnect.sock");
    let mut source = Source::new(&socket);

    let first = start(&socket);
    let before = pull(&mut source, Duration::from_millis(1500));
    assert!(
        before.len() >= 30,
        "frames from the first producer: {}",
        before.len()
    );
    assert_eq!(source.status().state, LinkState::Streaming);
    println!(
        "proof: first producer delivered {} frames, status {:?}",
        before.len(),
        source.status()
    );

    // The producer goes away. The source records it and waits out the backoff, without giving up.
    first.stop();
    let away = pull(&mut source, Duration::from_millis(2500));
    let status = source.status();
    println!(
        "proof: producer away for 2.5 s, delivered {}, status {status:?}",
        away.len()
    );
    assert!(away.is_empty(), "no frames while the producer is away");
    assert_eq!(status.state, LinkState::Waiting);
    assert!(
        status.failures >= 3,
        "reconnect attempts while away: {}",
        status.failures
    );
    assert!(status.next_delay.is_some(), "the next delay is reported");
    assert!(status.last_error.is_some(), "the last error is reported");

    // A new producer on the same socket: the same source delivers its frames again.
    let second = start(&socket);
    let resumed = pull(&mut source, Duration::from_millis(3000));
    let status = source.status();
    println!(
        "proof: producer returned, delivered {} frames, status {status:?}",
        resumed.len()
    );
    assert!(resumed.len() >= 30, "frames resumed: {}", resumed.len());
    assert_eq!(status.state, LinkState::Streaming);
    assert_eq!(status.connects, 2);
    assert!(
        resumed.windows(2).all(|pair| pair[0] < pair[1]),
        "the resumed frames arrive in publication order"
    );

    // A manual stop ends delivery and the connection.
    source.stop();
    assert_eq!(source.status().state, LinkState::Stopped);
    assert!(pull(&mut source, Duration::from_millis(200)).is_empty());
    second.stop();
}

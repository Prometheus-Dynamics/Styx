//! A producer over an existing pool of another metadata type, with an opaque part per frame.
//!
//! The pool is a `FramePool<u64>` over one memfd per buffer, as a camera's stream is over its own
//! descriptors. The producer publishes the pool's leases as they are (no copy), attaching each
//! frame's identity as the opaque part. A reader in another thread takes the frames, checks the
//! bytes and the opaque part, and the producer gives buffers back once readers let go.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Limits, Producer, Reader, SocketError};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
const BUFFERS: usize = 3;
const FRAMES: u64 = 6;

fn pattern(id: u64, at: usize) -> u8 {
    (id as u8).wrapping_mul(3).wrapping_add(at as u8)
}

fn pool() -> FramePool<u64> {
    let memories = (0..BUFFERS).map(|_| {
        let fd = shm_ring::sealed_memfd(PAYLOAD).expect("memfd");
        Box::new(FdMemory::memfd(fd, PAYLOAD, true).expect("map")) as Box<dyn BufferMemory>
    });
    FramePool::new(BufferSize::new(PAYLOAD, 0), memories).expect("pool")
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

fn socket_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-socket-pool-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn a_producer_publishes_an_existing_pool_of_another_type_with_its_opaque_part() {
    let pool = pool();
    let mut producer = Producer::with_pool(pool.clone(), 8).expect("producer over the pool");
    let path = socket_path();
    let listener = UnixListener::bind(&path).expect("bind");
    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let mut reader =
            Reader::connect(UnixStream::connect(reader_path).expect("connect")).expect("handshake");
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        // A reader that falls behind loses frames to the ring (counted as skipped), so the reader
        // waits for the newest frame, not for every one.
        while seen.last() != Some(&FRAMES) {
            assert!(std::time::Instant::now() < deadline, "the newest frame never came: {seen:?}");
            match reader.next_frame().expect("not cut off") {
                Some(frame) => {
                    let id = frame.lease().id();
                    let user = u64::from_le_bytes(frame.user().try_into().expect("8 bytes"));
                    assert_eq!(user, id * 10, "the opaque part is the producer's");
                    let read = frame.lease().read().expect("mapped");
                    let plane = read.plane(0).expect("plane");
                    assert!(
                        plane
                            .data
                            .iter()
                            .enumerate()
                            .all(|(at, b)| *b == pattern(id, at)),
                        "frame {id}: bytes differ"
                    );
                    seen.push(id);
                }
                None => {
                    let _ = reader.wait(Some(Duration::from_millis(5)));
                }
            }
        }
        seen
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(
            stream,
            Limits {
                max_held: 2,
                max_hold: None,
            },
        )
        .expect("accept the reader");

    for id in 1..=FRAMES {
        loop {
            producer.reclaim();
            let Ok(mut lease) = pool.acquire(entry(id)) else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            {
                let mut planes = lease.write().expect("a new lease is the sole owner");
                for (at, byte) in planes.bytes()[..PAYLOAD].iter_mut().enumerate() {
                    *byte = pattern(id, at);
                }
            }
            match producer.publish_with(lease, &(id * 10).to_le_bytes()) {
                Ok(_) => break,
                Err(SocketError::Buffer(_)) => thread::sleep(Duration::from_millis(1)),
                Err(err) => panic!("publish: {err}"),
            }
        }
    }
    let seen = reader.join().expect("reader");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "ids in order: {seen:?}");
    assert_eq!(seen.last(), Some(&FRAMES));

    // The reader is gone; the producer gives back every buffer but the newest frame's.
    producer.reclaim();
    let free = (0..BUFFERS).filter(|&i| pool.is_free(i)).count();
    assert_eq!(
        free,
        BUFFERS - 1,
        "all but the newest frame's buffer are back"
    );
    let copies: usize = styx_frame::copy_counters().iter().map(|c| c.copies()).sum();
    assert_eq!(copies, 0, "no copies");
}

#[test]
fn an_opaque_part_longer_than_the_limit_is_refused_and_nothing_is_published() {
    let pool = pool();
    let mut producer = Producer::with_pool(pool.clone(), 8).expect("producer");
    let lease = pool.acquire(entry(1)).expect("buffer");
    let too_long = vec![0_u8; styx_socket::USER_MAX + 1];
    assert!(matches!(
        producer.publish_with(lease, &too_long),
        Err(SocketError::Config(_))
    ));
    assert!(
        (0..BUFFERS).all(|i| pool.is_free(i)),
        "the refused lease's buffer is back in the pool"
    );
}

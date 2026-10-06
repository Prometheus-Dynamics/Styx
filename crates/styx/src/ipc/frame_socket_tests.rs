//! A camera stand-in with four buffers, reused as soon as they come back (oldest first, as the
//! PiSP back end does), feeding a [`FrameSocket`]; slow consumers check their frame is not
//! rewritten while they hold it.

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::smallvec;
use styx_core::prelude::*;

use super::{FrameSocket, FrameSocketOptions, fetch_frame};
#[allow(unused_imports)]
use styx_core::prelude::Hop;

const LEN: usize = 4096;

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "styx-frame-socket-{name}-{}.sock",
        std::process::id()
    ))
}

struct Camera {
    buffers: Vec<OwnedFd>,
    free: Mutex<VecDeque<usize>>,
    dropped: AtomicU64,
}

struct CameraBuffer {
    camera: Arc<Camera>,
    index: usize,
}

impl ExternalBacking for CameraBuffer {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        None
    }

    fn can_export(&self) -> bool {
        true
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let fd = self.camera.buffers[self.index]
            .try_clone()
            .map_err(FrameExportError::Fd)?;
        Ok(Some(FrameBackingExport::Memfd { fd, len: LEN }))
    }
}

impl Drop for CameraBuffer {
    fn drop(&mut self) {
        self.camera.free.lock().push_back(self.index);
    }
}

impl Camera {
    fn new(buffers: usize) -> Arc<Self> {
        let buffers = (0..buffers)
            .map(|_| {
                // SAFETY: creating an anonymous memory file; a non-negative result is ours.
                let fd = unsafe { libc::memfd_create(c"styx-camera".as_ptr(), libc::MFD_CLOEXEC) };
                assert!(fd >= 0);
                // SAFETY: `fd` was just created.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                File::from(fd.try_clone().unwrap())
                    .set_len(LEN as u64)
                    .unwrap();
                fd
            })
            .collect::<Vec<_>>();
        Arc::new(Self {
            free: Mutex::new((0..buffers.len()).collect()),
            buffers,
            dropped: AtomicU64::new(0),
        })
    }

    /// Frame `seq` (every byte `seq as u8`) in the oldest free buffer; none (dropped) when
    /// consumers hold them all. Never waits.
    fn capture(self: &Arc<Self>, seq: u64) -> Option<FrameLease> {
        let Some(index) = self.free.lock().pop_front() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let file = File::from(self.buffers[index].try_clone().unwrap());
        file.write_all_at(&[seq as u8; LEN], 0).unwrap();
        let res = Resolution::new(64, 64).unwrap();
        let meta = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb), seq);
        let layout = PlaneLayout {
            offset: 0,
            len: LEN,
            stride: 64,
        };
        Some(FrameLease::from_external(
            meta,
            smallvec![layout],
            Arc::new(CameraBuffer {
                camera: self.clone(),
                index,
            }),
        ))
    }
}

/// Publishes a frame every 2 ms until stopped; returns how many it published.
fn run_camera(
    camera: Arc<Camera>,
    socket: Arc<FrameSocket>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut seq = 1u64;
        let mut published = 0;
        while !stop.load(Ordering::Acquire) {
            if let Some(frame) = camera.capture(seq) {
                socket.publish(&frame).unwrap();
                published += 1;
            }
            seq += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        published
    })
}

fn bytes(frame: &FrameLease) -> Vec<u8> {
    frame.planes()[0].data().to_vec()
}

#[test]
fn a_held_frame_is_not_rewritten_and_the_camera_keeps_going() {
    let path = socket_path("held");
    let socket = Arc::new(FrameSocket::bind(&path).unwrap());
    let camera = Camera::new(4);
    let stop = Arc::new(AtomicBool::new(false));
    let producer = run_camera(camera.clone(), socket.clone(), stop.clone());

    // Two slow consumers each hold a frame for 150 frame periods.
    let held: Vec<FrameLease> = (0..2)
        .map(|_| {
            let f = fetch_frame(&path, Duration::from_secs(2)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            f
        })
        .collect();
    let first: Vec<Vec<u8>> = held.iter().map(bytes).collect();
    for b in &first {
        assert!(b.iter().all(|&v| v == b[0]), "one frame per buffer");
    }
    let published_before = socket.stats().published;
    std::thread::sleep(Duration::from_millis(300));
    for (f, b) in held.iter().zip(&first) {
        assert_eq!(&bytes(f), b, "rewritten while held");
    }
    // The camera ran on its other two buffers meanwhile.
    assert!(socket.stats().published > published_before + 50);
    assert_eq!(socket.stats().held_frames, 2);
    // A fast consumer still gets new frames.
    let fresh = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    assert!(fresh.meta().timestamp > held.iter().map(|f| f.meta().timestamp).max().unwrap());
    drop(fresh);
    drop(held);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        socket.stats().leases,
        0,
        "dropping the frames ends the leases"
    );
    assert_eq!(socket.stats().revoked, 0);
    stop.store(true, Ordering::Release);
    producer.join().unwrap();
}

#[test]
fn every_buffer_held_drops_frames_and_never_blocks() {
    let path = socket_path("all-held");
    let socket = Arc::new(FrameSocket::bind(&path).unwrap());
    let camera = Camera::new(2);
    let stop = Arc::new(AtomicBool::new(false));
    let producer = run_camera(camera.clone(), socket.clone(), stop.clone());
    let a = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    let mut b = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    while b.meta().timestamp == a.meta().timestamp {
        b = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    }
    // Both buffers held (one by each consumer, the latest one also by the socket).
    std::thread::sleep(Duration::from_millis(100));
    let dropped = camera.dropped.load(Ordering::Relaxed);
    assert!(
        dropped > 10,
        "the camera dropped frames ({dropped}) instead of waiting"
    );
    let (va, vb) = (bytes(&a), bytes(&b));
    drop(a);
    std::thread::sleep(Duration::from_millis(50));
    // The released buffer carries new frames; the held one does not change.
    assert_eq!(bytes(&b), vb);
    let c = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    assert_ne!(bytes(&c)[0], va[0]);
    stop.store(true, Ordering::Release);
    producer.join().unwrap();
}

#[test]
fn one_more_held_frame_than_allowed_ends_the_oldest_lease() {
    let path = socket_path("oldest");
    let options = FrameSocketOptions {
        max_held_frames: Some(2),
        ..Default::default()
    };
    let socket = FrameSocket::bind_with(&path, options).unwrap();
    let camera = Camera::new(5);
    let mut held = Vec::new();
    for seq in 1..=3 {
        socket.publish(&camera.capture(seq).unwrap()).unwrap();
        held.push(fetch_frame(&path, Duration::from_secs(2)).unwrap());
    }
    assert_eq!(socket.stats().held_frames, 2, "frame 3 is the latest");
    // Frame 4 makes three held besides the latest: frame 1's consumer loses its lease.
    socket.publish(&camera.capture(4).unwrap()).unwrap();
    let stats = socket.stats();
    assert_eq!((stats.held_frames, stats.leases, stats.revoked), (2, 2, 1));
    drop(held.remove(0));
    // Frames 2, 3 (held) and 4 (latest) keep their buffers; 1 and the fifth are free.
    assert_eq!(camera.free.lock().len(), 2);
}

#[test]
fn a_consumer_holding_too_long_loses_its_lease() {
    let path = socket_path("revoke");
    let options = FrameSocketOptions {
        max_hold: Some(Duration::from_millis(100)),
        ..Default::default()
    };
    let socket = FrameSocket::bind_with(&path, options).unwrap();
    let camera = Camera::new(4);
    socket.publish(&camera.capture(1).unwrap()).unwrap();
    let held = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    socket.publish(&camera.capture(2).unwrap()).unwrap();
    assert_eq!(socket.stats().held_frames, 1);
    let t = Instant::now();
    while socket.stats().revoked == 0 {
        assert!(t.elapsed() < Duration::from_secs(2), "not revoked");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(socket.stats().leases, 0);
    // Frame 1's buffer went back to the camera.
    assert_eq!(camera.free.lock().len(), 3);
    drop(held);
}

#[test]
fn consumers_closing_at_once_get_the_wire_format_helios_reads() {
    let path = socket_path("wire");
    let socket = FrameSocket::bind_with(
        &path,
        FrameSocketOptions {
            linger: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .unwrap();
    let camera = Camera::new(4);
    socket.publish(&camera.capture(9).unwrap()).unwrap();
    // As helios-engine reads it (Orion's fd frame): length, JSON, descriptors; then it closes.
    let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let mut header = [0u8; 4];
    std::io::Read::read_exact(&mut stream, &mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header) as usize];
    std::io::Read::read_exact(&mut stream, &mut payload).unwrap();
    drop(stream);
    // The bytes peers already read (golden: they must not change).
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        r#"{"descriptor":{"width":64,"height":64,"fourcc":"GREY","timestamp":9,"color":"Srgb","planes":[{"offset":0,"len":4096,"stride":64}]},"backing":{"kind":"memfd","len":4096}}"#
    );
    let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(json["backing"]["kind"], "memfd");
    assert_eq!(json["backing"]["len"], LEN);
    assert_eq!(json["descriptor"]["fourcc"], "GREY");
    assert_eq!(json["descriptor"]["color"], "Srgb");
    assert_eq!(json["descriptor"]["timestamp"], 9);
    assert_eq!(json["descriptor"]["planes"][0]["stride"], 64);
    // Lingering after the close, then let go.
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(socket.stats().leases, 1);
    socket.publish(&camera.capture(10).unwrap()).unwrap();
    let t = Instant::now();
    while socket.stats().leases > 0 {
        assert!(t.elapsed() < Duration::from_secs(2), "linger did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(camera.free.lock().len(), 3, "only the latest frame is held");
}

#[test]
fn a_consumer_before_the_first_frame_waits_for_it() {
    let path = socket_path("first");
    let socket = Arc::new(FrameSocket::bind(&path).unwrap());
    let camera = Camera::new(4);
    let fetch = {
        let path = path.clone();
        std::thread::spawn(move || fetch_frame(&path, Duration::from_secs(2)))
    };
    std::thread::sleep(Duration::from_millis(50));
    socket.publish(&camera.capture(5).unwrap()).unwrap();
    let frame = fetch.join().unwrap().unwrap();
    assert_eq!(frame.meta().timestamp, 5);
    assert_eq!(bytes(&frame)[0], 5);
}

/// Connects as a raw consumer and reads the frame message (header and payload); the
/// connection stays open.
fn raw_fetch(path: &std::path::Path) -> std::os::unix::net::UnixStream {
    use std::io::Read;
    let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header) as usize];
    stream.read_exact(&mut payload).unwrap();
    stream
}

/// The server closed the connection: end of stream, or a reset when the consumer's byte was
/// still unread.
fn assert_closed(stream: &mut std::os::unix::net::UnixStream) {
    match std::io::Read::read(stream, &mut [0u8; 1]) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("connection not closed: {other:?}"),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let t = Instant::now();
    while !done() {
        assert!(t.elapsed() < Duration::from_secs(2), "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn any_byte_or_a_close_releases_the_lease() {
    use std::io::Write;
    let path = socket_path("release");
    let socket = FrameSocket::bind(&path).unwrap();
    let camera = Camera::new(4);
    socket.publish(&camera.capture(1).unwrap()).unwrap();
    let mut a = raw_fetch(&path);
    let b = raw_fetch(&path);
    socket.publish(&camera.capture(2).unwrap()).unwrap();
    assert_eq!((socket.stats().leases, socket.stats().held_frames), (2, 1));
    // A byte releases the lease; the server then closes the connection.
    a.write_all(&[0]).unwrap();
    wait_until("byte did not release", || socket.stats().leases == 1);
    assert_closed(&mut a);
    // Closing (as the kernel does for a consumer that exits) releases the other.
    drop(b);
    wait_until("close did not release", || socket.stats().leases == 0);
    // Frame 1's buffer is back with the camera; frame 2 (the latest) is still kept.
    assert_eq!(camera.free.lock().len(), 3);
    assert_eq!(socket.stats().revoked, 0);
}

#[test]
fn at_max_hold_the_connection_closes_and_the_pixels_may_change() {
    let path = socket_path("expiry");
    let options = FrameSocketOptions {
        max_hold: Some(Duration::from_millis(100)),
        ..Default::default()
    };
    let socket = FrameSocket::bind_with(&path, options).unwrap();
    let camera = Camera::new(2);
    socket.publish(&camera.capture(1).unwrap()).unwrap();
    let held = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    let mut raw = raw_fetch(&path);
    socket.publish(&camera.capture(2).unwrap()).unwrap();
    assert!(camera.capture(3).is_none(), "both buffers held: dropped");
    // The consumer sees its connection closed at the cutoff.
    let t = Instant::now();
    assert_closed(&mut raw);
    assert!(t.elapsed() < Duration::from_secs(1));
    wait_until("not revoked", || socket.stats().revoked == 2);
    // Its frame's buffer is the camera's again: the next capture writes it while the
    // consumer may still have it mapped.
    assert_eq!(bytes(&held)[0], 1);
    socket.publish(&camera.capture(4).unwrap()).unwrap();
    assert_eq!(bytes(&held)[0], 4, "rewritten after the cutoff");
}

#[test]
fn consumers_get_the_newest_frame_and_nothing_queued() {
    let path = socket_path("newest");
    let options = FrameSocketOptions {
        first_frame_wait: Duration::from_millis(50),
        ..Default::default()
    };
    let socket = FrameSocket::bind_with(&path, options).unwrap();
    let camera = Camera::new(4);
    for seq in 1..=3 {
        socket.publish(&camera.capture(seq).unwrap()).unwrap();
    }
    // Frames 1 and 2 were never served: one connection, one frame, the latest.
    let first = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    assert_eq!(first.meta().timestamp, 3);
    // Nothing new published: the same frame again (same timestamp, same buffer).
    let again = fetch_frame(&path, Duration::from_secs(2)).unwrap();
    assert_eq!(again.meta().timestamp, 3);
    drop((first, again));
    // No frame to offer: the consumer waits `first_frame_wait`, then the server closes.
    socket.clear();
    let t = Instant::now();
    assert!(fetch_frame(&path, Duration::from_secs(2)).is_err());
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(socket.stats().unserved, 1);
    assert_eq!(socket.stats().published, 3);
}

/// Writes frame messages (memfd and dma-buf planes) to `$STYX_FUZZ_SEEDS/frame_socket_message/`
/// as seeds for the `frame_socket_message` fuzz target (`docs/fuzzing.md`): a shape byte
/// (descriptor count and size, see the target) and the JSON.
#[test]
#[ignore = "writes fuzz seeds; run with STYX_FUZZ_SEEDS set"]
fn write_fuzz_seeds() {
    use styx_core::lease_codec::{
        LeaseBacking as Backing, LeaseMessage as Message, LeasePlane as Plane,
    };
    let Some(dir) = std::env::var_os("STYX_FUZZ_SEEDS") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("frame_socket_message");
    std::fs::create_dir_all(&dir).unwrap();
    let nv12 = FrameLease::from_visible_bytes(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(64, 32).unwrap(),
            ColorSpace::Bt709,
        ),
        7,
        &[1; 64 * 48],
    )
    .unwrap();
    let descriptor = nv12.descriptor();
    // In one memfd, the chroma plane follows the luma plane.
    let mut packed = descriptor.clone();
    packed.planes[1].offset = 64 * 32;
    let messages = [
        // One 4096-byte memfd.
        ("memfd", 0b1000, packed, Backing::Memfd { len: 64 * 48 }),
        // Two 4096-byte descriptors, one per plane.
        (
            "dmabuf",
            0b1001,
            descriptor,
            Backing::DmabufPlanes {
                planes: vec![
                    Plane {
                        offset: 0,
                        len: 2048,
                    },
                    Plane {
                        offset: 0,
                        len: 1024,
                    },
                ],
            },
        ),
    ];
    let mut hops = FrameHops::new();
    hops.set_sequence(Some(42));
    for (hop, ns) in [
        (Hop::Sensor, 1_000),
        (Hop::Dequeued, 9_000),
        (Hop::Sent, 12_000),
    ] {
        hops.set(hop, ns);
    }
    for (name, shape, descriptor, backing) in messages {
        for (suffix, hops) in [("", None), ("-hops", Some(HopRecord::new(None, &hops)))] {
            let mut bytes = vec![shape];
            serde_json::to_writer(
                &mut bytes,
                &Message {
                    descriptor: descriptor.clone(),
                    backing: backing.clone(),
                    hops,
                },
            )
            .unwrap();
            std::fs::write(dir.join(format!("{name}{suffix}")), bytes).unwrap();
        }
    }
}

/// A frame with hops is sent with them and the send time; the consumer adds its receive and
/// import times; the server's statistics (hold times, hops) are read from `<path>.stats`.
#[test]
fn hops_travel_with_the_frame_and_statistics_are_served() {
    use super::{FrameFetcher, fetch_metrics, fetch_metrics_text, stats_path};
    let path = socket_path("hops");
    let socket = FrameSocket::bind(&path).unwrap();
    assert!(stats_path(&path).exists());
    let camera = Camera::new(4);
    let mut frame = camera.capture(3).unwrap();
    let now = TimestampClock::Monotonic.now_ns().unwrap();
    let hops = &mut frame.meta_mut().hops;
    hops.set_sequence(Some(3));
    hops.set(Hop::Sensor, now - 9_000_000);
    hops.set(Hop::Dequeued, now - 4_000_000);
    hops.set(Hop::Queued, now - 1_000_000);
    socket.publish(&frame).unwrap();
    let mut fetcher = FrameFetcher::new(&path);
    let fetched = fetcher.fetch(Duration::from_secs(2)).unwrap();
    let record = fetched.meta().hop_record();
    assert_eq!(record.sequence, Some(3));
    assert_eq!(fetched.meta().sequence(), Some(3));
    let (sent, received, imported) = (
        record.sent.unwrap(),
        record.received.unwrap(),
        record.imported.unwrap(),
    );
    assert!(record.queued.unwrap() <= sent && sent <= received && received <= imported);
    assert_eq!(bytes(&fetched).len(), LEN);
    drop(fetched);
    wait_until("the lease ends", || socket.stats().leases == 0);
    let consumer = fetcher.hop_metrics();
    let names: Vec<_> = consumer.hops.iter().map(|h| h.to.as_str()).collect();
    assert_eq!(
        names,
        ["dequeued", "queued", "sent", "received", "imported"]
    );
    // The server, read as another process would.
    let m = fetch_metrics(&path).unwrap();
    assert_eq!((m.published, m.served), (1, 1));
    assert_eq!(m.hold.total, 1);
    assert_eq!(m.hops.frames, 1);
    assert!(m.hops.hop("sent").is_some());
    let text = fetch_metrics_text(&path).unwrap();
    assert!(text.contains("styx_frame_socket_events_total"), "{text}");
    assert!(text.contains("styx_consumer_hop_ms"), "{text}");
    assert!(text.contains("styx_frame_socket_hold_ms"), "{text}");
    // A frame without hops goes out as before: no "hops" member.
    socket.publish(&camera.capture(4).unwrap()).unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let mut header = [0u8; 4];
    std::io::Read::read_exact(&mut stream, &mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header) as usize];
    std::io::Read::read_exact(&mut stream, &mut payload).unwrap();
    assert!(!std::str::from_utf8(&payload).unwrap().contains("hops"));
    drop(socket);
    assert!(!stats_path(&path).exists());
}

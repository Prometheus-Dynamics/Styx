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

/// Writes frame messages (memfd and dma-buf planes) to `$STYX_FUZZ_SEEDS/frame_socket_message/`
/// as seeds for the `frame_socket_message` fuzz target (`docs/fuzzing.md`): a shape byte
/// (descriptor count and size, see the target) and the JSON.
#[test]
#[ignore = "writes fuzz seeds; run with STYX_FUZZ_SEEDS set"]
fn write_fuzz_seeds() {
    use super::{Backing, Message, Plane};
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
    for (name, shape, descriptor, backing) in messages {
        let mut bytes = vec![shape];
        serde_json::to_writer(
            &mut bytes,
            &Message {
                descriptor,
                backing,
            },
        )
        .unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

//! Proof for dma-buf pools across a real process boundary.
//!
//! The parent publishes an existing `FramePool` whose buffers are dma-bufs: two buffers at
//! offsets in one dma-buf, one in another, so the handshake's descriptor dedupe is exercised. A
//! child process (this test binary, re-run) takes frames on commands from its stdin and answers
//! on its stdout. The proof checks that the child's frames are dma-buf frames with the parent's
//! bytes, that dma-buf sync brackets are made with no refusals, that no copy happens in either
//! process, that a held buffer comes back once the child lets go, and that a child killed while
//! holding frames returns its buffers too.
//!
//! Needs `/dev/dma_heap/system` or `/dev/udmabuf`. Without either the test prints why and passes.

#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lemnos_linux_sys::{dma_heap, udmabuf};
use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{
    BufferMemory, BufferSize, CpuAccess, FdMemory, FrameEntry, FramePool, MemoryKind,
    copy_counters, packed_layouts,
};
use styx_socket::{BeginError, Frame, Limits, Producer, Reader};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
/// One GREY frame.
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// GPU slack after each frame, so a buffer is a page: dma-buf allocations and udmabufs are whole
/// pages.
const SLACK: usize = 4096 - PAYLOAD;
/// One buffer: payload and slack.
const BUFFER: usize = PAYLOAD + SLACK;
const CHILD: &str = "STYX_SOCKET_DMABUF_CHILD";

/// The byte at `at` of frame `id`: what the parent writes and the child checks.
fn expected(id: u64, at: usize) -> u8 {
    (id as u8).wrapping_mul(3).wrapping_add(at as u8)
}

fn socket_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-socket-{}-{name}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

/// A dma-buf of `len` bytes: from the system dma-heap, else a udmabuf over a sealed memfd.
/// `None`, with the reasons printed, when neither works here.
fn dmabuf(len: usize) -> Option<OwnedFd> {
    let mut reasons = Vec::new();
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dma_heap/system")
    {
        Ok(heap) => match dma_heap::allocate(heap.as_fd(), len as u64) {
            Ok(fd) => return Some(fd),
            Err(err) => reasons.push(format!("dma_heap allocate: {err}")),
        },
        Err(err) => reasons.push(format!("/dev/dma_heap/system: {err}")),
    }
    match udmabuf_over_memfd(len) {
        Ok(fd) => return Some(fd),
        Err(err) => reasons.push(format!("udmabuf: {err}")),
    }
    println!("skipped: no dma-buf here ({})", reasons.join("; "));
    None
}

fn udmabuf_over_memfd(len: usize) -> std::io::Result<OwnedFd> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")?;
    let memfd = udmabuf::memfd_create(c"styx-socket-dmabuf")?;
    std::fs::File::from(memfd.try_clone()?).set_len(len as u64)?;
    udmabuf::add_seals(memfd.as_fd(), udmabuf::SEAL_SHRINK)?;
    udmabuf::create(dev.as_fd(), memfd.as_fd(), 0, len as u64)
}

/// Dma-buf sync brackets the parent makes, and the kernel's refusals of them.
#[derive(Debug, Default)]
struct Brackets {
    brackets: AtomicU64,
    errors: AtomicU64,
}

/// A buffer of the parent's pool: a dma-buf slice, with its CPU brackets counted.
struct Counted {
    memory: FdMemory,
    brackets: Arc<Brackets>,
}

impl Counted {
    fn bracket(&self, call: impl FnOnce(&FdMemory)) {
        let before = self.memory.sync_errors();
        call(&self.memory);
        self.brackets.brackets.fetch_add(1, Ordering::Relaxed);
        let refused = self.memory.sync_errors().saturating_sub(before);
        self.brackets
            .errors
            .fetch_add(refused as u64, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to an `FdMemory`, which upholds `BufferMemory`'s contract for its
// own mapping. The sync brackets only count and forward.
unsafe impl BufferMemory for Counted {
    fn kind(&self) -> MemoryKind {
        self.memory.kind()
    }
    fn len(&self) -> usize {
        self.memory.len()
    }
    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        self.memory.cpu_ptr()
    }
    fn cpu_writable(&self) -> bool {
        self.memory.cpu_writable()
    }
    fn begin_cpu(&self, access: CpuAccess) {
        self.bracket(|memory| memory.begin_cpu(access));
    }
    fn end_cpu(&self, access: CpuAccess) {
        self.bracket(|memory| memory.end_cpu(access));
    }
    fn fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.memory.fd()
    }
    fn fd_offset(&self) -> usize {
        self.memory.fd_offset()
    }
}

fn counted(fd: &OwnedFd, offset: usize, brackets: &Arc<Brackets>) -> Box<dyn BufferMemory> {
    let memory = FdMemory::dmabuf_at(fd.try_clone().expect("dup"), offset, BUFFER, true)
        .expect("map the dma-buf writable");
    Box::new(Counted {
        memory,
        brackets: Arc::clone(brackets),
    })
}

/// Takes a buffer, writes frame `id`'s bytes into it, and publishes it. `false` when the
/// producer dropped the frame for want of a free buffer.
fn publish(producer: &mut Producer, id: u64) -> bool {
    match producer.begin(entry(id)) {
        Ok(mut lease) => {
            {
                let mut planes = lease
                    .write()
                    .expect("a new frame's lease is the sole owner");
                for (at, byte) in planes.bytes()[..PAYLOAD].iter_mut().enumerate() {
                    *byte = expected(id, at);
                }
            }
            producer.publish(lease).expect("publish");
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

/// The child's side of a frame: a dma-buf frame, with the parent's bytes, read in place.
fn check(frame: &Frame) {
    let lease = frame.lease();
    assert_eq!(lease.memory_kind(), MemoryKind::Dmabuf, "a dma-buf frame");
    assert!(
        lease.memory().fd().is_some(),
        "the frame names its dma-buf for a GPU import"
    );
    let read = lease.read().expect("the buffer is mapped for the CPU");
    let plane = read.plane(0).expect("one plane");
    assert_eq!(plane.data.len(), PAYLOAD);
    assert!(
        plane
            .data
            .iter()
            .enumerate()
            .all(|(at, byte)| *byte == expected(lease.id(), at)),
        "frame {} bytes are the parent's",
        lease.id()
    );
}

fn command(stdin: &mut ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("tell the child");
    stdin.flush().expect("flush");
}

/// Reads the child's output until a line containing `marker`, and returns that line.
fn expect_line(out: &mut BufReader<ChildStdout>, marker: &str) -> String {
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            out.read_line(&mut line).expect("child output"),
            0,
            "the child ended before printing {marker:?}"
        );
        if line.contains(marker) {
            return line;
        }
    }
}

fn field(line: &str, key: &str) -> u64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
        .unwrap_or_else(|| panic!("{key} in {line:?}"))
}

/// The child process: a reader that does what its stdin says. It does nothing unless the parent
/// started it with the socket's path in the environment.
#[test]
fn dmabuf_reader_child() {
    let Some(path) = std::env::var_os(CHILD) else {
        return;
    };
    let mut reader = Reader::connect(UnixStream::connect(path).expect("connect")).expect("connect");
    let mut held: Vec<Frame> = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("a command");
        let mut words = line.split_whitespace();
        match words.next() {
            Some("take") => {
                let count: usize = words.next().expect("a count").parse().expect("a count");
                let target = held.len() + count;
                let started = Instant::now();
                while held.len() < target {
                    match reader.next_frame().expect("not cut off") {
                        Some(frame) => {
                            check(&frame);
                            held.push(frame);
                        }
                        None => {
                            assert!(
                                started.elapsed() < Duration::from_secs(5),
                                "the parent published no frame"
                            );
                            let _ = reader.wait(Some(Duration::from_millis(5)));
                        }
                    }
                }
                println!("TOOK {}", held.len());
            }
            Some("drop") => {
                let dropped = held.len();
                held.clear();
                println!("DROPPED {dropped}");
            }
            Some("stats") => {
                let stats = reader.stats();
                let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
                println!(
                    "STATS delivered={} held={} dma_brackets={} dma_sync_errors={} copies={copies}",
                    stats.delivered, stats.held, stats.dma_brackets, stats.dma_sync_errors
                );
            }
            Some("exit") => break,
            other => panic!("an unknown command {other:?}"),
        }
        std::io::stdout().flush().expect("flush");
    }
}

#[test]
fn a_dmabuf_pool_is_published_across_a_process_boundary() {
    let Some(first) = dmabuf(2 * BUFFER) else {
        return;
    };
    let Some(second) = dmabuf(BUFFER) else {
        return;
    };
    let path = socket_path("dmabuf");
    let listener = UnixListener::bind(&path).expect("bind");
    let brackets = Arc::new(Brackets::default());
    let memories = vec![
        counted(&first, 0, &brackets),
        counted(&first, BUFFER, &brackets),
        counted(&second, 0, &brackets),
    ];
    let pool: FramePool<()> =
        FramePool::new(BufferSize::new(PAYLOAD, SLACK), memories).expect("the pool");
    let mut producer = Producer::with_pool(pool.clone(), 8).expect("a producer over the pool");
    assert_eq!(
        producer.pool().descriptors().len(),
        2,
        "two buffers share the first dma-buf: two descriptors in all"
    );
    assert_eq!(pool.memory(0).expect("buffer 0").kind(), MemoryKind::Dmabuf);

    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "dmabuf_reader_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("reader process");
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("handshake");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut out = BufReader::new(child.stdout.take().expect("child stdout"));

    // Frames 1 and 2 go to the child, which holds them. Frame 3 takes the third buffer. Frame 4
    // finds every buffer held or newest, and is dropped without waiting.
    assert!(publish(&mut producer, 1));
    assert!(publish(&mut producer, 2));
    command(&mut stdin, "take 2");
    expect_line(&mut out, "TOOK 2");
    assert!(publish(&mut producer, 3), "frame 3 is published");
    assert!(
        !publish(&mut producer, 4),
        "frame 4 is dropped while the child holds frames 1 and 2"
    );
    assert_eq!(producer.stats().dropped, 1);

    // The child lets go: its buffers come back, and frame 5 takes one.
    command(&mut stdin, "drop");
    expect_line(&mut out, "DROPPED 2");
    assert!(
        publish(&mut producer, 5),
        "frame 5 is published once the child let go"
    );
    command(&mut stdin, "take 2");
    expect_line(&mut out, "TOOK 2");

    command(&mut stdin, "stats");
    let stats = expect_line(&mut out, "STATS");
    println!("child: {}", stats.trim());
    assert_eq!(
        field(&stats, "dma_sync_errors"),
        0,
        "no dma-buf sync refused"
    );
    assert!(
        field(&stats, "dma_brackets") > 0,
        "the child synced its dma-bufs"
    );
    assert_eq!(field(&stats, "copies"), 0, "no copy in the child");

    // The child dies holding frames 3 and 5. Its socket closes; the producer takes its holds
    // away on the next begin, and frames 6 to 8 go in.
    child.kill().expect("kill the child");
    child.wait().expect("reap the child");
    assert!(
        publish(&mut producer, 6),
        "frame 6 after the child was killed"
    );
    assert!(publish(&mut producer, 7));
    assert!(publish(&mut producer, 8));
    let after = producer.stats();
    println!("producer: {after:?}");
    assert_eq!((after.readers, after.gone, after.dropped), (0, 1, 1));

    let parent = (
        brackets.brackets.load(Ordering::Relaxed),
        brackets.errors.load(Ordering::Relaxed),
    );
    println!("parent: dma brackets {} sync errors {}", parent.0, parent.1);
    assert!(parent.0 > 0, "the parent synced its dma-bufs");
    assert_eq!(parent.1, 0, "no dma-buf sync refused in the parent");
    assert!(
        copy_counters().iter().all(|c| c.copies() == 0),
        "no copy in the parent"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_pool_of_host_memory_cannot_be_published_across_processes() {
    let pool: FramePool<()> = FramePool::host(2, BufferSize::new(PAYLOAD, SLACK)).expect("pool");
    assert!(
        matches!(
            Producer::with_pool(pool, 8),
            Err(styx_socket::SocketError::Config(_))
        ),
        "host memory has no descriptor to send"
    );
}

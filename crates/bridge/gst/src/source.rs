//! The Rust half: a [`Source`] reads frames from one producer and says what each frame is. It
//! reconnects by itself: when the producer goes away it waits out the backoff, and when a producer
//! returns it connects and carries on, with the caller's element still running.

use std::ffi::CStr;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shm_fdpass as fdpass;
use styx_format::FourCc;
use styx_frame::{Layouts, PlaneLayout};
use styx_socket::{Frame, ReadError, Reader, StreamEnd};

use crate::link::{Backoff, LinkState, LinkStatus};

/// The GStreamer caps name of a Styx fourcc: a raw video format name, or `image/jpeg` for MJPG.
/// `None` for a format GStreamer has no name for (H.264 among them): such frames are counted and
/// dropped.
pub fn gst_format(code: FourCc) -> Option<&'static CStr> {
    let code = match code {
        FourCc::R8 => FourCc::GREY,
        FourCc::RGB3 => FourCc::RG24,
        FourCc::BGR3 => FourCc::BG24,
        other => other,
    };
    Some(match code {
        FourCc::YUYV => c"YUY2",
        FourCc::UYVY => c"UYVY",
        FourCc::YVYU => c"YVYU",
        FourCc::NV12 => c"NV12",
        FourCc::NV21 => c"NV21",
        FourCc::YU12 | FourCc::I420 => c"I420",
        FourCc::YV12 => c"YV12",
        FourCc::GREY => c"GRAY8",
        FourCc::RG24 => c"RGB",
        FourCc::BG24 => c"BGR",
        FourCc::RGBA => c"RGBA",
        FourCc::BGRA => c"BGRA",
        FourCc::MJPG => c"image/jpeg",
        _ => return None,
    })
}

/// Counts for one source.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SourceStats {
    /// Frames handed on.
    pub delivered: u64,
    /// Frames dropped because the format has no GStreamer name.
    pub unsupported: u64,
    /// Frames dropped because the memory is neither mapped nor a descriptor.
    pub unmapped: u64,
}

/// One frame, held: the element hands it to GStreamer and releases it when GStreamer frees the
/// memory. Its bytes are the producer's, where it wrote them.
#[derive(Debug)]
pub struct Delivered {
    frame: Frame,
    format: &'static CStr,
    pts_ns: u64,
}

impl Delivered {
    /// The producer's frame id (the entry id it published).
    pub fn id(&self) -> u64 {
        self.frame.lease().id()
    }

    /// The GStreamer caps name: a raw format such as `NV12`, or `image/jpeg`.
    pub const fn format(&self) -> &'static CStr {
        self.format
    }

    /// Whether the frame is compressed (`image/jpeg`): one plane, of the frame's own length.
    pub fn compressed(&self) -> bool {
        self.format.to_bytes() == b"image/jpeg"
    }

    /// The frame's time since the source's first frame, nanoseconds.
    pub const fn pts_ns(&self) -> u64 {
        self.pts_ns
    }

    pub fn width(&self) -> u32 {
        self.frame.lease().format().resolution.width.get()
    }

    pub fn height(&self) -> u32 {
        self.frame.lease().format().resolution.height.get()
    }

    /// The frame's planes: where each starts in the buffer, and its row stride.
    pub fn layouts(&self) -> &Layouts {
        self.frame.lease().layouts()
    }

    /// The whole buffer's bytes, where the producer wrote them. `None` when not mapped.
    pub fn data(&self) -> Option<&[u8]> {
        let memory = self.frame.lease().memory();
        let ptr = memory.cpu_ptr()?;
        // SAFETY: `cpu_ptr` is the start of `len()` bytes that stay valid while the buffer's
        // memory is held (the frame lease keeps it alive for as long as `self`), and the
        // producer does not write a buffer a reader holds, so nothing writes these bytes here.
        Some(unsafe { std::slice::from_raw_parts(ptr.as_ptr(), memory.len()) })
    }

    /// Bytes in the whole buffer the frame lives in, mapped or not.
    pub fn buffer_len(&self) -> usize {
        self.frame.lease().memory().len()
    }

    /// The bytes GStreamer should see: the frame's own length for a compressed frame (its one
    /// plane, which ends where the encoder ended), the whole buffer for a raw one.
    pub fn payload_len(&self) -> usize {
        if self.compressed() {
            self.layouts()
                .span()
                .unwrap_or_else(|| self.buffer_len())
                .min(self.buffer_len())
        } else {
            self.buffer_len()
        }
    }

    /// The buffer's descriptor (a memfd or dma-buf), `None` when there is none.
    pub fn raw_fd(&self) -> Option<i32> {
        self.frame.lease().memory().fd().map(|fd| fd.as_raw_fd())
    }

    /// Plane `index`'s layout, `None` past the last plane.
    pub fn plane(&self, index: usize) -> Option<PlaneLayout> {
        self.layouts().get(index).copied()
    }
}

/// One producer's frames, read by one reader. The producer is a socket path; the source connects
/// when it can and reconnects with backoff when the connection is lost. Nothing here blocks past
/// the wait the caller gives.
#[derive(Debug)]
pub struct Source {
    socket: PathBuf,
    reader: Option<Reader>,
    backoff: Backoff,
    status: LinkStatus,
    first_ns: Option<u64>,
    stats: SourceStats,
    stopped: bool,
}

impl Source {
    /// A source for the producer at `socket`. Nothing is connected yet.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            reader: None,
            backoff: Backoff::new(),
            status: LinkStatus::default(),
            first_ns: None,
            stats: SourceStats::default(),
            stopped: false,
        }
    }

    /// The producer's socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Whether a producer is connected now.
    pub fn connected(&self) -> bool {
        self.reader.is_some()
    }

    pub const fn stats(&self) -> SourceStats {
        self.stats
    }

    /// The link's status: whether a producer is connected, the connects and failures so far, the
    /// delay until the next attempt while waiting, and the last failure.
    pub fn status(&self) -> LinkStatus {
        self.status.clone()
    }

    /// Stops the source: it drops its connection and delivers nothing more. The element calls this
    /// when it is stopped.
    pub fn stop(&mut self) {
        self.stopped = true;
        self.reader = None;
        self.status.state = LinkState::Stopped;
        self.status.next_delay = None;
    }

    /// The next frame, held, or `None` when none came within `wait` (or the producer is not
    /// connected yet, or the frame was dropped as unsupported).
    pub fn next(&mut self, wait: Duration) -> Option<Delivered> {
        if self.stopped {
            std::thread::sleep(wait);
            return None;
        }
        let now = Instant::now();
        if !self.connect_at(now) {
            std::thread::sleep(wait);
            return None;
        }
        let reader = self.reader.as_mut()?;
        if !peer_open(reader.socket()) {
            self.fail(now, "the producer closed its socket".into());
            std::thread::sleep(wait);
            return None;
        }
        match reader.next_frame() {
            Ok(Some(frame)) => self.deliver(frame),
            Ok(None) => {
                let _ = reader.wait(Some(wait));
                None
            }
            // The producer closed its ring: the end of its stream, which is normal, not a failure.
            Err(ReadError::Ended(StreamEnd::Closed)) => {
                self.ended(now);
                None
            }
            Err(err) => {
                self.fail(now, err.to_string());
                None
            }
        }
    }

    /// Connects when an attempt is due at `now`. `true` when connected. The clock is an argument,
    /// so the backoff is tested without sleeping: [`Source::next`] calls this with the real time.
    pub fn connect_at(&mut self, now: Instant) -> bool {
        if self.reader.is_some() {
            return true;
        }
        if self.stopped || !self.backoff.due(now) {
            return false;
        }
        self.status.state = LinkState::Connecting;
        match open(&self.socket) {
            Ok(reader) => {
                self.reader = Some(reader);
                self.status.connects += 1;
                self.status.state = LinkState::Streaming;
                self.status.next_delay = None;
                self.backoff.reset();
                true
            }
            Err(why) => {
                self.fail(now, why);
                false
            }
        }
    }

    /// The producer closed its ring cleanly: its stream ended, with every frame delivered. Not a
    /// failure: the reader is let go, no error is kept, and the next attempt comes after the first
    /// backoff step.
    fn ended(&mut self, now: Instant) {
        self.reader = None;
        self.backoff.reset();
        self.status.state = LinkState::Waiting;
        self.status.next_delay = Some(self.backoff.failed(now));
        self.status.last_error = None;
    }

    fn fail(&mut self, now: Instant, why: String) {
        self.reader = None;
        self.status.failures += 1;
        self.status.state = LinkState::Waiting;
        self.status.next_delay = Some(self.backoff.failed(now));
        self.status.last_error = Some(why);
    }

    fn deliver(&mut self, frame: Frame) -> Option<Delivered> {
        let lease = frame.lease();
        let Some(format) = gst_format(lease.format().code) else {
            self.stats.unsupported += 1;
            return None;
        };
        if lease.memory().cpu_ptr().is_none() && lease.memory().fd().is_none() {
            self.stats.unmapped += 1;
            return None;
        }
        let stamp = lease.timestamp_ns();
        let first = *self.first_ns.get_or_insert(stamp);
        self.stats.delivered += 1;
        Some(Delivered {
            frame,
            format,
            pts_ns: stamp.saturating_sub(first),
        })
    }
}

fn open(socket: &Path) -> Result<Reader, String> {
    let stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    Reader::connect(stream).map_err(|err| err.to_string())
}

/// Whether the producer still holds its end of `socket`. A producer never sends after its
/// handshake, so this only looks for end of stream (the producer makes the same check for its
/// readers). Never blocks.
fn peer_open(socket: &UnixStream) -> bool {
    !matches!(fdpass::recv(socket.as_fd(), &mut [], 0, true), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_formats_have_gstreamer_names_and_mjpg_is_jpeg() {
        assert_eq!(gst_format(FourCc::NV12), Some(c"NV12"));
        assert_eq!(gst_format(FourCc::YUYV), Some(c"YUY2"));
        assert_eq!(gst_format(FourCc::GREY), Some(c"GRAY8"));
        assert_eq!(gst_format(FourCc::R8), Some(c"GRAY8"));
        assert_eq!(gst_format(FourCc::MJPG), Some(c"image/jpeg"));
        assert_eq!(gst_format(FourCc::H264), None);
    }

    #[test]
    fn a_missing_producer_backs_off_on_a_fake_clock_without_sleeping() {
        let t0 = Instant::now();
        let mut source = Source::new("/nonexistent/styx-gst-test.sock");
        assert!(!source.connect_at(t0));
        assert!(!source.connected());
        assert_eq!(source.status().state, LinkState::Waiting);
        assert_eq!(source.status().failures, 1);
        assert_eq!(source.status().next_delay, Some(Duration::from_millis(250)));
        assert!(source.status().last_error.is_some());

        // Inside the window no new connection is tried.
        assert!(!source.connect_at(t0 + Duration::from_millis(249)));
        assert_eq!(source.status().failures, 1);

        let mut now = t0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            now += source.status().next_delay.expect("waiting");
            assert!(!source.connect_at(now));
            seen.push(source.status().next_delay.expect("waiting").as_millis());
        }
        assert_eq!(seen, [500, 1000, 2000, 4000, 5000, 5000]);
        assert_eq!(source.status().failures, 7);
        assert_eq!(source.status().connects, 0);
    }

    #[test]
    fn a_stopped_source_connects_no_more() {
        let t0 = Instant::now();
        let mut source = Source::new("/nonexistent/styx-gst-stop.sock");
        source.stop();
        assert!(!source.connect_at(t0));
        assert_eq!(source.status().state, LinkState::Stopped);
        assert_eq!(source.status().failures, 0);
    }
}

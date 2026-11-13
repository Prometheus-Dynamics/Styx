//! The Rust half: a [`Source`] reads frames from one producer and says what each frame is.

use std::ffi::CStr;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use styx_format::FourCc;
use styx_frame::{Layouts, PlaneLayout};
use styx_socket::{Frame, Reader};

const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// The GStreamer raw video format name of a Styx fourcc, `None` when GStreamer has no name for
/// it (compressed formats, and formats with no raw equivalent).
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
    /// Connections made to the producer.
    pub connects: u64,
    /// Connections lost or refused (each one backs off before the next try).
    pub failures: u64,
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

    /// The GStreamer raw format name, e.g. `NV12`.
    pub const fn format(&self) -> &'static CStr {
        self.format
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
/// when it can and reconnects with backoff when the connection is lost.
#[derive(Debug)]
pub struct Source {
    socket: PathBuf,
    reader: Option<Reader>,
    retry_at: Instant,
    backoff: Duration,
    first_ns: Option<u64>,
    stats: SourceStats,
}

impl Source {
    /// A source for the producer at `socket`. Nothing is connected yet.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            reader: None,
            retry_at: Instant::now(),
            backoff: BACKOFF_START,
            first_ns: None,
            stats: SourceStats::default(),
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

    /// The next frame, held, or `None` when none came within `wait` (or the producer is not
    /// connected yet, or the frame was dropped as unsupported).
    pub fn next(&mut self, wait: Duration) -> Option<Delivered> {
        if self.reader.is_none() && !self.connect() {
            std::thread::sleep(wait);
            return None;
        }
        let reader = self.reader.as_mut()?;
        match reader.next_frame() {
            Ok(Some(frame)) => self.deliver(frame),
            Ok(None) => {
                let _ = reader.wait(Some(wait));
                None
            }
            Err(_) => {
                self.lost();
                None
            }
        }
    }

    fn connect(&mut self) -> bool {
        if Instant::now() < self.retry_at {
            return false;
        }
        let connected = UnixStream::connect(&self.socket)
            .map_err(|_| ())
            .and_then(|stream| Reader::connect(stream).map_err(|_| ()));
        match connected {
            Ok(reader) => {
                self.reader = Some(reader);
                self.stats.connects += 1;
                self.backoff = BACKOFF_START;
                true
            }
            Err(()) => {
                self.failed();
                false
            }
        }
    }

    fn lost(&mut self) {
        self.reader = None;
        self.failed();
    }

    fn failed(&mut self) {
        self.stats.failures += 1;
        self.retry_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_CAP);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_formats_have_gstreamer_names() {
        assert_eq!(gst_format(FourCc::NV12), Some(c"NV12"));
        assert_eq!(gst_format(FourCc::YUYV), Some(c"YUY2"));
        assert_eq!(gst_format(FourCc::GREY), Some(c"GRAY8"));
        assert_eq!(gst_format(FourCc::R8), Some(c"GRAY8"));
        assert_eq!(gst_format(FourCc::MJPG), None);
        assert_eq!(gst_format(FourCc::H264), None);
    }

    #[test]
    fn a_source_with_no_producer_backs_off_and_delivers_nothing() {
        let mut source = Source::new("/nonexistent/styx-gst-test.sock");
        assert!(source.next(Duration::from_millis(1)).is_none());
        assert!(!source.connected());
        let stats = source.stats();
        assert_eq!(stats.connects, 0);
        assert_eq!(stats.failures, 1);
        // Inside the backoff window no new connection is tried.
        assert!(source.next(Duration::from_millis(1)).is_none());
        assert_eq!(source.stats().failures, 1);
    }
}

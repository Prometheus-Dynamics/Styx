//! The frame path's own costs, with relaxed atomics only (`path-metrics` feature, on with
//! `std`):
//!
//! * [`HopCounters`]: windows of the time between consecutive [`Hop`]s of the frames a
//!   capture, a socket or a consumer saw, and how many of those frames were copied.
//! * [`copied`] / [`copied_frame`]: the one call every place that copies pixels makes, counted
//!   process-wide by [`CopySite`] ([`path_counters`]) and on the frame itself
//!   ([`FrameHops::copied`]).
//! * [`synced`] / [`timed_sync`]: dma-buf cache maintenance (`DMA_BUF_IOCTL_SYNC`) calls and
//!   their time, process-wide.
//! * [`frame_mapped`] / [`PathCounters::cpu_reads`]: frame memory mapped for the CPU, and
//!   bracketed CPU reads of frames.
//! * [`pool_exhausted`]: a pool or a capture had no free buffer (every one held).
//! * [`moved`] / [`traffic_counters`]: the bytes each [`TrafficStage`] read and wrote in
//!   memory (the receiver, the ISP and its extra passes, CPU pyramid levels, copies, the GPU),
//!   process-wide: computed from the buffers each job touches until the SoC's bus counters
//!   are read.

use core::time::Duration;

use super::camera::Ring;
use crate::buffer::{FrameHops, FrameMeta, HOP_COUNT, Hop};
use crate::sync::Counter;

/// Where pixels were copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CopySite {
    /// The software ISP staging raw input rows (receiver buffers mapped uncached).
    SoftIsp = 0,
    /// A conversion or decode into new memory (YUYV to NV12, NV12 to RGB, JPEG to pixels).
    Conversion = 1,
    /// A region, crop or view copied (realigned) into new memory.
    Region = 2,
    /// A frame not in shareable memory copied into a memfd to send it to another process.
    MemfdExport = 3,
    /// A frame copied into owned host memory (`materialize_owned`, `to_visible_vec`).
    Materialize = 4,
    /// A backend copying a frame out of the driver's buffer (V4L2 without zero copy, UVC
    /// payloads assembled into a frame).
    Capture = 5,
    /// A raw frame kept for a still.
    Raw = 6,
    /// Anything else (scaling into new memory: overviews the ISP does not make).
    Other = 7,
}

/// Copy sites counted.
pub const COPY_SITES: usize = 8;

impl CopySite {
    /// Every site.
    pub const ALL: [CopySite; COPY_SITES] = [
        CopySite::SoftIsp,
        CopySite::Conversion,
        CopySite::Region,
        CopySite::MemfdExport,
        CopySite::Materialize,
        CopySite::Capture,
        CopySite::Raw,
        CopySite::Other,
    ];

    /// Snake-case name, as metric labels use it.
    pub const fn name(self) -> &'static str {
        match self {
            CopySite::SoftIsp => "softisp",
            CopySite::Conversion => "conversion",
            CopySite::Region => "region",
            CopySite::MemfdExport => "memfd_export",
            CopySite::Materialize => "materialize",
            CopySite::Capture => "capture",
            CopySite::Raw => "raw",
            CopySite::Other => "other",
        }
    }
}

/// Process-wide copy, sync and pool counters ([`path_counters`]).
#[derive(Debug)]
pub struct PathCounters {
    copies: [Counter; COPY_SITES],
    copied_bytes: [Counter; COPY_SITES],
    /// `DMA_BUF_IOCTL_SYNC` calls (start and end each count).
    pub syncs: Counter,
    /// Time spent in them, nanoseconds.
    pub sync_ns: Counter,
    /// `mmap` calls frame backings made to read frame memory on the CPU (lazily, on the first
    /// read of a buffer they had not mapped).
    pub frame_maps: Counter,
    /// Bracketed CPU reads asked of frames ([`FrameLease::begin_cpu_read`]: Daedalus's
    /// `daedalus:frame` `plane_data`).
    ///
    /// [`FrameLease::begin_cpu_read`]: crate::buffer::FrameLease::begin_cpu_read
    pub cpu_reads: Counter,
    /// A pool or a capture had no free buffer: every one was held (a frame dropped or a buffer
    /// allocated instead).
    pub pool_exhausted: Counter,
}

static PATH: PathCounters = PathCounters {
    copies: [const { Counter::new() }; COPY_SITES],
    copied_bytes: [const { Counter::new() }; COPY_SITES],
    syncs: Counter::new(),
    sync_ns: Counter::new(),
    frame_maps: Counter::new(),
    cpu_reads: Counter::new(),
    pool_exhausted: Counter::new(),
};

/// The process's copy, sync and pool counters.
pub fn path_counters() -> &'static PathCounters {
    &PATH
}

impl PathCounters {
    /// Copies made at `site`.
    pub fn copies(&self, site: CopySite) -> u64 {
        self.copies[site as usize].get()
    }

    /// Bytes copied at `site`.
    pub fn copied_bytes(&self, site: CopySite) -> u64 {
        self.copied_bytes[site as usize].get()
    }

    /// All copies.
    pub fn total_copies(&self) -> u64 {
        self.copies.iter().map(Counter::get).sum()
    }

    /// All bytes copied.
    pub fn total_copied_bytes(&self) -> u64 {
        self.copied_bytes.iter().map(Counter::get).sum()
    }
}

/// A stage that reads and writes frame memory ([`moved`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TrafficStage {
    /// The receiver writing raw frames (and its statistics) to memory.
    Receiver = 0,
    /// An ISP's main job: raw in, its outputs, statistics and denoise history.
    Isp = 1,
    /// An ISP's extra passes (regions, pyramid levels), each reading the raw frame again.
    IspPass = 2,
    /// The software ISP on the CPU: raw in, outputs out.
    CpuIsp = 3,
    /// Pyramid levels box-filtered on the CPU: each level read, the next written.
    CpuPyramid = 4,
    /// Pixels copied on the CPU ([`copied`]): read once, written once.
    CpuCopy = 5,
    /// GPU stages: frames read, results written.
    Gpu = 6,
    /// Frames scaled on the CPU (previews, overviews the ISP does not make).
    CpuScale = 7,
}

/// Traffic stages counted.
pub const TRAFFIC_STAGES: usize = 8;

impl TrafficStage {
    /// Every stage.
    pub const ALL: [TrafficStage; TRAFFIC_STAGES] = [
        TrafficStage::Receiver,
        TrafficStage::Isp,
        TrafficStage::IspPass,
        TrafficStage::CpuIsp,
        TrafficStage::CpuPyramid,
        TrafficStage::CpuCopy,
        TrafficStage::Gpu,
        TrafficStage::CpuScale,
    ];

    /// Snake-case name, as metric labels use it.
    pub const fn name(self) -> &'static str {
        match self {
            TrafficStage::Receiver => "receiver",
            TrafficStage::Isp => "isp",
            TrafficStage::IspPass => "isp_pass",
            TrafficStage::CpuIsp => "cpu_isp",
            TrafficStage::CpuPyramid => "cpu_pyramid",
            TrafficStage::CpuCopy => "cpu_copy",
            TrafficStage::Gpu => "gpu",
            TrafficStage::CpuScale => "cpu_scale",
        }
    }

    /// What does the work: `hardware` (a device's DMA), `cpu` or `gpu`.
    pub const fn unit(self) -> &'static str {
        match self {
            TrafficStage::Receiver | TrafficStage::Isp | TrafficStage::IspPass => "hardware",
            TrafficStage::CpuIsp
            | TrafficStage::CpuPyramid
            | TrafficStage::CpuCopy
            | TrafficStage::CpuScale => "cpu",
            TrafficStage::Gpu => "gpu",
        }
    }
}

/// Process-wide memory traffic by stage ([`traffic_counters`]).
#[derive(Debug)]
pub struct TrafficCounters {
    jobs: [Counter; TRAFFIC_STAGES],
    read: [Counter; TRAFFIC_STAGES],
    written: [Counter; TRAFFIC_STAGES],
}

static TRAFFIC: TrafficCounters = TrafficCounters {
    jobs: [const { Counter::new() }; TRAFFIC_STAGES],
    read: [const { Counter::new() }; TRAFFIC_STAGES],
    written: [const { Counter::new() }; TRAFFIC_STAGES],
};

/// The process's memory traffic by stage.
pub fn traffic_counters() -> &'static TrafficCounters {
    &TRAFFIC
}

impl TrafficCounters {
    /// Jobs `stage` ran (a frame, a pass, a level, a copy).
    pub fn jobs(&self, stage: TrafficStage) -> u64 {
        self.jobs[stage as usize].get()
    }

    /// Bytes `stage` read from memory.
    pub fn read(&self, stage: TrafficStage) -> u64 {
        self.read[stage as usize].get()
    }

    /// Bytes `stage` wrote to memory.
    pub fn written(&self, stage: TrafficStage) -> u64 {
        self.written[stage as usize].get()
    }
}

/// Counts one job of `stage` that read `read` and wrote `written` bytes of memory.
#[inline]
pub fn moved(stage: TrafficStage, read: u64, written: u64) {
    TRAFFIC.jobs[stage as usize].incr();
    TRAFFIC.read[stage as usize].add(read);
    TRAFFIC.written[stage as usize].add(written);
}

/// Counts a copy of `bytes` bytes of pixels at `site` (process-wide). Prefer [`copied_frame`]
/// where the copy's output frame is at hand, so the frame carries the count to its consumer.
#[inline]
pub fn copied(site: CopySite, bytes: usize) {
    PATH.copies[site as usize].incr();
    PATH.copied_bytes[site as usize].add(bytes as u64);
    moved(TrafficStage::CpuCopy, bytes as u64, bytes as u64);
}

/// Counts a copy of `bytes` bytes at `site` that produced the frame `meta` describes:
/// process-wide and in the frame's hops, which carry it to wherever the frame is delivered.
#[inline]
pub fn copied_frame(meta: &mut FrameMeta, site: CopySite, bytes: usize) {
    copied(site, bytes);
    meta.hops.copied(bytes);
}

/// Counts one `DMA_BUF_IOCTL_SYNC` that took `ns`.
#[inline]
pub fn synced(ns: u64) {
    PATH.syncs.incr();
    PATH.sync_ns.add(ns);
}

/// Runs `sync` (one `DMA_BUF_IOCTL_SYNC`) and counts it with its time.
#[cfg(feature = "std")]
#[inline]
pub fn timed_sync<R>(sync: impl FnOnce() -> R) -> R {
    let start = std::time::Instant::now();
    let r = sync();
    synced(start.elapsed().as_nanos() as u64);
    r
}

/// Counts one `mmap` a frame backing made to read frame memory on the CPU.
#[inline]
pub fn frame_mapped() {
    PATH.frame_maps.incr();
}

/// A pool or a capture found every buffer held.
#[inline]
pub fn pool_exhausted() {
    PATH.pool_exhausted.incr();
}

/// Windows of the time between consecutive hops of the frames recorded, and their copies.
///
/// [`HopCounters::record`] takes a frame's [`FrameHops`] once, where its record is complete
/// for the path being measured (an in-process consumer taking it; a consumer process having
/// imported it): for each recorded hop after the first, the time since the hop recorded before
/// it goes into that hop's window, and the first to the last into [`HopCounters::total`].
#[derive(Debug)]
pub struct HopCounters {
    /// Index `h`: time from the previous recorded hop to hop `h` (`Hop::ALL[h]`); index 0
    /// (the sensor, which has none before it) stays empty.
    into: [Ring; HOP_COUNT],
    /// First to last recorded hop of each frame.
    pub total: Ring,
    /// Which hop came before each hop in the latest frame (`u8::MAX`: none yet).
    from: [portable_atomic::AtomicU8; HOP_COUNT],
    /// Frames recorded.
    pub frames: Counter,
    /// Of those, frames whose pixels were copied on the way, and the bytes.
    pub copied_frames: Counter,
    pub copied_bytes: Counter,
    /// Copies (a frame may be copied more than once).
    pub copies: Counter,
}

impl Default for HopCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl HopCounters {
    /// Nothing recorded.
    pub const fn new() -> Self {
        Self {
            into: [const { Ring::new() }; HOP_COUNT],
            total: Ring::new(),
            from: [const { portable_atomic::AtomicU8::new(u8::MAX) }; HOP_COUNT],
            frames: Counter::new(),
            copied_frames: Counter::new(),
            copied_bytes: Counter::new(),
            copies: Counter::new(),
        }
    }

    /// Records a frame's hops and copies (see the type's documentation).
    #[inline]
    pub fn record(&self, hops: &FrameHops) {
        use portable_atomic::Ordering::Relaxed;
        self.frames.incr();
        let copies = hops.copies();
        if copies > 0 {
            self.copied_frames.incr();
            self.copies.add(u64::from(copies));
            self.copied_bytes.add(hops.copied_bytes());
        }
        let mut first: Option<u64> = None;
        let mut previous: Option<(Hop, u64)> = None;
        for hop in Hop::ALL {
            let Some(ns) = hops.get(hop) else {
                continue;
            };
            if let Some((from, at)) = previous
                && let Some(d) = ns.checked_sub(at)
            {
                self.into[hop.index()].push(d);
                if self.from[hop.index()].load(Relaxed) != from as u8 {
                    self.from[hop.index()].store(from as u8, Relaxed);
                }
            }
            first.get_or_insert(ns);
            previous = Some((hop, ns));
        }
        if let (Some(first), Some((_, last))) = (first, previous)
            && last > first
        {
            self.total.push(last - first);
        }
    }

    /// The window of times into `hop` from the hop before it, and which hop that was in the
    /// latest frame (`None`: nothing recorded into `hop`).
    pub fn window_into(&self, hop: Hop) -> Option<(Hop, &Ring)> {
        let from = self.from[hop.index()].load(portable_atomic::Ordering::Relaxed);
        let from = *Hop::ALL.get(usize::from(from))?;
        let ring = &self.into[hop.index()];
        (ring.count() > 0).then_some((from, ring))
    }

    /// Each hop with samples: `(from, to, window)`, in path order.
    pub fn windows(&self) -> impl Iterator<Item = (Hop, Hop, &Ring)> + '_ {
        Hop::ALL
            .into_iter()
            .filter_map(|to| self.window_into(to).map(|(from, ring)| (from, to, ring)))
    }

    /// Adds a time into `hop` from `from` measured elsewhere (e.g. a consumer's receive and
    /// import times reported back to the server).
    pub fn push(&self, from: Hop, hop: Hop, d: Duration) {
        self.into[hop.index()].push_duration(d);
        self.from[hop.index()].store(from as u8, portable_atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each stage counts its jobs and bytes; a copy counts as CPU traffic, read and written.
    #[test]
    fn traffic_counts_jobs_and_bytes_by_stage() {
        let t = traffic_counters();
        let before = (t.jobs(TrafficStage::IspPass), t.read(TrafficStage::IspPass));
        let copy = (
            t.read(TrafficStage::CpuCopy),
            t.written(TrafficStage::CpuCopy),
        );
        moved(TrafficStage::IspPass, 1000, 10);
        copied(CopySite::Other, 64);
        assert!(t.jobs(TrafficStage::IspPass) > before.0);
        assert!(t.read(TrafficStage::IspPass) >= before.1 + 1000);
        assert!(t.read(TrafficStage::CpuCopy) >= copy.0 + 64);
        assert!(t.written(TrafficStage::CpuCopy) >= copy.1 + 64);
        assert_eq!(TrafficStage::ALL.len(), TRAFFIC_STAGES);
        assert_eq!(TrafficStage::IspPass.unit(), "hardware");
    }

    #[test]
    fn hop_windows_follow_the_recorded_hops() {
        let c = HopCounters::new();
        let mut h = FrameHops::new();
        h.set(Hop::Sensor, 1_000);
        h.set(Hop::Dequeued, 9_000);
        h.set(Hop::Queued, 10_000);
        h.set(Hop::Taken, 10_500);
        h.copied(64);
        c.record(&h);
        assert_eq!(c.frames.get(), 1);
        assert_eq!((c.copied_frames.get(), c.copied_bytes.get()), (1, 64));
        let w: Vec<_> = c
            .windows()
            .map(|(a, b, r)| (a, b, r.quantile(0.5).unwrap()))
            .collect();
        assert_eq!(
            w,
            [
                (Hop::Sensor, Hop::Dequeued, 8_000),
                (Hop::Dequeued, Hop::Queued, 1_000),
                (Hop::Queued, Hop::Taken, 500),
            ]
        );
        assert_eq!(c.total.quantile(1.0), Some(9_500));
        assert!(c.window_into(Hop::IspDone).is_none());
        let before = path_counters().copies(CopySite::Region);
        copied(CopySite::Region, 10);
        assert_eq!(path_counters().copies(CopySite::Region), before + 1);
    }
}

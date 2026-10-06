//! Live per-capture metrics, recorded on the frame path with relaxed atomics only (no locks, no
//! allocation). The counters, the windows of the last [`WINDOW`] samples (frame intervals,
//! latencies, ISP times) and the 3A, AF and still state are the runtime's
//! (`styx_core::metrics::Counters`, `no_std`); this module feeds them from frame metadata
//! and adds what only a Linux process has: buffers held by consumers, consumers, worker CPU
//! time, reconnecting captures. Snapshots (`super::camera`) read them on demand.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

pub(crate) use styx_core::metrics::AaaSample;
pub use styx_core::metrics::WINDOW;
pub(crate) use styx_core::metrics::{Counters as RuntimeCounters, FrameSample, HopCounters, Ring};
use styx_core::prelude::{BackendFrameMeta, ExternalBacking, FrameMeta, Hop};

use super::camera::Window;

/// A ring's window as a snapshot shows it.
pub(crate) trait RingWindow {
    fn window(&self) -> Window;
}

impl RingWindow for Ring {
    fn window(&self) -> Window {
        Window::from_samples(self.samples(), self.count(), self.max_ever())
    }
}

/// Buffers handed to consumers and not yet returned, and how long they were held.
#[derive(Default)]
pub(crate) struct BufferStats {
    pub(crate) held: AtomicI64,
    pub(crate) held_bytes: AtomicI64,
    pub(crate) peak_held: AtomicI64,
    pub(crate) hold: Ring,
}

/// One consumer of a shared capture or of the camera service.
#[derive(Default)]
pub(crate) struct ConsumerStats {
    pub(crate) label: Mutex<String>,
    pub(crate) received: AtomicU64,
    /// Frames this consumer did not get (its queue overflowed, its socket was full).
    pub(crate) dropped: AtomicU64,
    /// Frames it holds now.
    pub(crate) held: AtomicI64,
    pub(crate) hold: Ring,
    /// Hop times and copies of the frames sent to it (camera service clients).
    pub(crate) hops: HopCounters,
    /// Where the consumer's own drop counters live, read at snapshot time (queue evictions).
    pub(crate) extra_drops: OnceLock<Box<dyn Fn() -> u64 + Send + Sync>>,
}

impl ConsumerStats {
    pub(crate) fn new(label: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            label: Mutex::new(label.into()),
            ..Self::default()
        })
    }

    /// A consumer of `capture` receiving on `queues` (its own, and its group's): frames those
    /// queues dropped (replaced by newer ones) are frames it did not get.
    pub(crate) fn for_queues<const N: usize>(
        capture: &CaptureMetrics,
        label: String,
        queues: [styx_core::queue::BoundedRx<styx_core::prelude::FrameLease>; N],
    ) -> Arc<Self> {
        let stats = Self::new(label);
        let _ = stats.extra_drops.set(Box::new(move || {
            queues.iter().map(|q| q.stats().evictions).sum()
        }));
        capture.add_consumer(&stats);
        stats
    }

    /// A frame sent to (and now held by) a camera service client.
    pub(crate) fn sent(&self) {
        self.received.fetch_add(1, Relaxed);
        self.held.fetch_add(1, Relaxed);
    }

    /// The client released a frame it held for `held`.
    pub(crate) fn released(&self, held: Duration) {
        self.held.fetch_sub(1, Relaxed);
        self.hold.push(held.as_nanos() as u64);
    }

    /// A frame the consumer did not get.
    pub(crate) fn dropped(&self) {
        self.dropped.fetch_add(1, Relaxed);
    }

    /// Count a frame handed to the consumer.
    #[inline]
    pub(crate) fn count<T>(
        &self,
        outcome: styx_core::prelude::RecvOutcome<T>,
    ) -> styx_core::prelude::RecvOutcome<T> {
        if matches!(outcome, styx_core::prelude::RecvOutcome::Data(_)) {
            self.received.fetch_add(1, Relaxed);
        }
        outcome
    }

    pub(crate) fn snapshot(&self) -> super::camera::ConsumerMetrics {
        let extra = self.extra_drops.get().map_or(0, |f| f());
        super::camera::ConsumerMetrics {
            label: self.label.lock().map(|l| l.clone()).unwrap_or_default(),
            received: self.received.load(Relaxed),
            dropped: self.dropped.load(Relaxed) + extra,
            held: self.held.load(Relaxed).max(0) as u64,
            hold: self.hold.window(),
            hops: (self.hops.frames.get() > 0).then(|| super::path::HopMetrics::of(&self.hops)),
        }
    }
}

/// What a capture counts besides the runtime's counters.
#[derive(Default)]
pub(crate) struct Counters {
    /// Frames, drops by cause, windows, 3A and still state (`styx_core::metrics`).
    pub(crate) rt: RuntimeCounters,
    /// Sequence gaps: the runtime's tracking, or the backend's own (V4L2, libcamera), into
    /// the counter the capture handle shares.
    pub(crate) sequence_gaps: Arc<AtomicU64>,
    pub(crate) cpu_ns_retired: AtomicU64,
    /// Hop times of the frames consumers took, and their copies.
    pub(crate) hops: HopCounters,
}

/// Static facts about a capture, set when it starts.
#[derive(Clone, Debug, Default)]
pub(crate) struct Info {
    pub name: String,
    pub backend: String,
    pub mode: String,
    pub configured_fps: Option<f64>,
    pub isp: Option<&'static str>,
}

pub(crate) struct Live {
    pub(crate) id: u64,
    pub(crate) started: Instant,
    pub(crate) info: Mutex<Info>,
    pub(crate) counters: Counters,
    /// Gaps in the sequence are frames dropped as damaged (UVC), not lost by the sensor.
    gaps_are_corrupt: AtomicBool,
    pub(crate) buffers: Arc<BufferStats>,
    /// Worker threads (Linux thread ids) and their CPU time when last read.
    threads: Mutex<Vec<(i32, u64)>>,
    /// For a reconnecting capture: the backend capture running now.
    pub(crate) current: Mutex<Option<CaptureMetrics>>,
    pub(crate) attached: OnceLock<super::camera::Attached>,
    pub(crate) consumers: Mutex<Vec<Weak<ConsumerStats>>>,
}

/// Live metrics of one capture: cheap to clone, recorded on the frame path, read by
/// [`CaptureHandle::camera_metrics`](crate::capture_api::CaptureHandle::camera_metrics) and
/// [`snapshot`](super::snapshot).
#[derive(Clone)]
pub struct CaptureMetrics(pub(crate) Arc<Live>);

impl Default for CaptureMetrics {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(Arc::new(Live {
            id: NEXT_ID.fetch_add(1, Relaxed),
            started: Instant::now(),
            info: Mutex::new(Info::default()),
            counters: Counters::default(),
            gaps_are_corrupt: AtomicBool::new(false),
            buffers: Arc::default(),
            threads: Mutex::new(Vec::new()),
            current: Mutex::new(None),
            attached: OnceLock::new(),
            consumers: Mutex::new(Vec::new()),
        }))
    }
}

impl std::fmt::Debug for CaptureMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureMetrics")
            .field("id", &self.0.id)
            .finish_non_exhaustive()
    }
}

impl CaptureMetrics {
    /// For a backend that counts sequence gaps itself (into `gaps`; the V4L2 backend).
    #[cfg_attr(not(feature = "v4l2"), allow(dead_code))]
    pub(crate) fn with_sequence_gaps(gaps: Arc<AtomicU64>) -> Self {
        let mut m = Self::default();
        let live = Arc::get_mut(&mut m.0).expect("new");
        live.counters.sequence_gaps = gaps;
        live.counters.rt.set_track_sequence(false);
        m
    }

    /// For a backend whose sequence gaps are frames it dropped as damaged (UVC).
    #[cfg_attr(not(feature = "uvc"), allow(dead_code))]
    pub(crate) fn gaps_are_corrupt(self) -> Self {
        self.0.gaps_are_corrupt.store(true, Relaxed);
        self
    }

    /// The sequence gap counter, for [`CaptureHandle`](crate::capture_api::CaptureHandle)'s
    /// health report.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn sequence_gaps(&self) -> Arc<AtomicU64> {
        self.0.counters.sequence_gaps.clone()
    }

    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn set_isp(&self, isp: &'static str) {
        if let Ok(mut info) = self.0.info.lock() {
            info.isp = Some(isp);
        }
    }

    /// A still request finished: `Some((latency, shots, landed))`, or `None` when it failed.
    pub(crate) fn still(&self, done: Option<(Duration, u64, u64)>) {
        self.0.counters.rt.stills.record(done);
    }

    /// Count the calling thread's CPU time as this capture's (its worker threads).
    pub(crate) fn register_thread(&self) {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: gettid has no preconditions.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            if let Ok(mut threads) = self.0.threads.lock() {
                threads.push((tid, 0));
            }
        }
    }

    /// CPU time of the worker threads (their last reading for threads that have ended).
    pub(crate) fn cpu_ns(&self) -> (u64, usize) {
        let Ok(mut threads) = self.0.threads.lock() else {
            return (0, 0);
        };
        let mut total = 0;
        for (tid, last) in threads.iter_mut() {
            if let Some(ns) = thread_cpu_ns(*tid) {
                *last = (*last).max(ns);
            }
            total += *last;
        }
        (
            total + self.0.counters.cpu_ns_retired.load(Relaxed),
            threads.len(),
        )
    }

    /// Record a frame the backend is about to deliver: frame interval, sensor-to-delivery
    /// latency, sequence gaps and receiver errors, sensor exposure and gain.
    #[inline]
    pub(crate) fn frame(&self, meta: &FrameMeta) {
        let l = &*self.0;
        let (sequence, error, exposure) = match &meta.backend {
            Some(BackendFrameMeta::Native(n)) => (
                Some(n.sequence),
                n.error,
                Some((n.exposure_ns, n.analog_gain, n.digital_gain)),
            ),
            Some(BackendFrameMeta::Uvc(u)) => (Some(u.sequence), u.error, None),
            // V4L2_BUF_FLAG_ERROR
            Some(BackendFrameMeta::V4l2(v)) => (Some(v.sequence), v.flags & 0x40 != 0, None),
            _ => (None, false, None),
        };
        let lost = l.counters.rt.frame(&FrameSample {
            sequence: sequence.map(u64::from),
            timestamp_ns: meta.timestamp,
            now_ns: now_ns(meta),
            corrupt: error,
            exposure,
        });
        self.gaps(lost);
    }

    fn gaps(&self, gaps: u64) {
        if gaps == 0 {
            return;
        }
        let l = &*self.0;
        let before = if l.gaps_are_corrupt.load(Relaxed) {
            let c = &l.counters.rt.corrupted;
            let before = c.get();
            c.add(gaps);
            before
        } else {
            l.counters.sequence_gaps.fetch_add(gaps, Relaxed)
        };
        // Logged at 1, 2, 4, 8... lost frames: never once per frame.
        if (before + gaps).ilog2() != before.checked_ilog2().unwrap_or(u32::MAX) {
            crate::trace::info!(
                capture = l.id,
                lost = before + gaps,
                corrupt = l.gaps_are_corrupt.load(Relaxed),
                "frames missing from the sequence"
            );
        }
    }

    /// Record a frame the consumer took from the capture queue.
    #[inline]
    pub(crate) fn received(&self, meta: &FrameMeta) {
        self.0.counters.rt.received(meta.timestamp, now_ns(meta));
    }

    /// A frame the consumer took: its [`Hop::Taken`] stamped, its hops (sensor to taken) and
    /// copies recorded.
    #[inline]
    pub(crate) fn taken(&self, meta: &mut FrameMeta) {
        meta.hops.mark(Hop::Taken);
        self.0.counters.hops.record(&meta.hops);
        self.received(meta);
    }

    /// The ISP dropped a frame (all its output buffers were held by consumers).
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn isp_skipped(&self) {
        styx_core::metrics::pool_exhausted();
        self.0.counters.rt.isp_skipped();
    }

    /// Time the ISP spent on a frame (`isp`: the back end job or the software ISP pass) and the
    /// whole of its processing (`processing`: raw frame dequeued to outputs ready).
    #[inline]
    pub(crate) fn isp_time(&self, isp: Duration, processing: Duration) {
        self.0.counters.rt.isp_time(isp, processing);
    }

    #[inline]
    pub(crate) fn aaa(&self, s: &AaaSample) {
        self.0.counters.rt.aaa.record(s);
    }

    pub(crate) fn aaa_state(&self) -> Option<super::camera::AaaState> {
        let a = self.0.counters.rt.aaa.read()?;
        let (exposure, analogue, digital) = match a.exposure {
            Some((e, ag, dg)) => (Some(e.as_nanos() as f64 / 1000.0), Some(ag), Some(dg)),
            None => (None, None, None),
        };
        let mut state = super::camera::AaaState {
            ae_state: a
                .ae_locked
                .map(|l| if l { "converged" } else { "searching" }.into()),
            exposure_us: exposure,
            analogue_gain: analogue,
            digital_gain: digital,
            colour_temperature_k: a.colour_temperature,
            lux: a.lux,
            awb_converged: a.awb_converged,
            flicker_hz: a.flicker_period.map(|p| 1e6 / p.as_micros() as f64),
            ..Default::default()
        };
        if let Some(af) = &a.af {
            super::af::fill(af, &mut state);
        }
        Some(state)
    }

    /// `backing` as one of this capture's buffers: counted as held until its frame (and every
    /// share of it) is dropped, and how long that took recorded.
    pub(crate) fn track<B: ExternalBacking + 'static>(&self, backing: B) -> MeteredBacking<B> {
        let bytes = backing.backing_bytes().unwrap_or(0) as i64;
        let stats = self.0.buffers.clone();
        let held = stats.held.fetch_add(1, Relaxed) + 1;
        stats.peak_held.fetch_max(held, Relaxed);
        stats.held_bytes.fetch_add(bytes, Relaxed);
        MeteredBacking {
            inner: backing,
            stats,
            since: Instant::now(),
            bytes,
        }
    }

    /// Add `previous`'s counters to these (a reconnecting capture replacing its backend capture).
    pub(crate) fn absorb(&self, previous: &CaptureMetrics) {
        let (from, to) = (&previous.0.counters, &self.0.counters);
        to.rt.absorb(&from.rt);
        if !Arc::ptr_eq(&from.sequence_gaps, &to.sequence_gaps) {
            to.sequence_gaps.fetch_add(previous.gap_count(), Relaxed);
        }
        let (cpu, _) = previous.cpu_ns();
        to.cpu_ns_retired.fetch_add(cpu, Relaxed);
    }

    /// For a reconnecting capture: the backend capture now running (`None` while reconnecting).
    pub(crate) fn set_current(&self, current: Option<CaptureMetrics>) {
        if let Ok(mut c) = self.0.current.lock() {
            *c = current;
        }
    }

    pub(crate) fn add_consumer(&self, consumer: &Arc<ConsumerStats>) {
        if let Ok(mut consumers) = self.0.consumers.lock() {
            consumers.retain(|c| c.strong_count() > 0);
            consumers.push(Arc::downgrade(consumer));
        }
    }

    /// Frames are recorded where the backend produces them.
    pub(crate) fn has_producer(&self) -> bool {
        self.0.counters.rt.has_producer()
    }
}

/// Stamps the hops a frame entering the consumer queue has passed: [`Hop::Sensor`] from its
/// timestamp (on `CLOCK_MONOTONIC`) and [`Hop::Dequeued`] from its capture instant when the
/// backend did not set them, and [`Hop::Queued`] now.
#[inline]
pub(crate) fn stamp_queued(meta: &mut FrameMeta) {
    if meta.hops.get(Hop::Sensor).is_none()
        && let Some(ns) = meta.sensor_monotonic_ns()
    {
        meta.hops.set(Hop::Sensor, ns);
    }
    if meta.hops.get(Hop::Dequeued).is_none()
        && let Some(at) = meta.capture_instant
    {
        meta.hops.set(Hop::Dequeued, at.as_nanos());
    }
    if meta.hops.sequence().is_none() {
        meta.hops.set_sequence(meta.sequence());
    }
    meta.hops.mark(Hop::Queued);
}

/// The frame's clock now, when it has a timestamp to measure from.
#[inline]
fn now_ns(meta: &FrameMeta) -> Option<u64> {
    if meta.timestamp == 0 {
        return None;
    }
    meta.clock.and_then(|c| c.now_ns())
}

/// CPU time of thread `tid` of this process, in nanoseconds: `schedstat`'s run time, else
/// `stat`'s user and system ticks.
pub(crate) fn thread_cpu_ns(tid: i32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let base = format!("/proc/self/task/{tid}");
        if let Ok(s) = std::fs::read_to_string(format!("{base}/schedstat"))
            && let Some(ns) = s.split_whitespace().next().and_then(|v| v.parse().ok())
        {
            return Some(ns);
        }
        let stat = std::fs::read_to_string(format!("{base}/stat")).ok()?;
        ticks_ns(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = tid;
        None
    }
}

/// User plus system time of a `/proc/.../stat` line, in nanoseconds.
pub(crate) fn ticks_ns(stat: &str) -> Option<u64> {
    let rest: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let user: u64 = rest.get(11)?.parse().ok()?;
    let system: u64 = rest.get(12)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    #[cfg(target_os = "linux")]
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    #[cfg(not(target_os = "linux"))]
    let hz = 100;
    Some((user + system) * 1_000_000_000 / hz)
}

/// A capture buffer counted while frames hold it (see [`CaptureMetrics::track`]).
pub(crate) struct MeteredBacking<B> {
    inner: B,
    stats: Arc<BufferStats>,
    since: Instant,
    bytes: i64,
}

impl<B> Drop for MeteredBacking<B> {
    fn drop(&mut self) {
        self.stats.held.fetch_sub(1, Relaxed);
        self.stats.held_bytes.fetch_sub(self.bytes, Relaxed);
        self.stats.hold.push(self.since.elapsed().as_nanos() as u64);
    }
}

impl<B: ExternalBacking> ExternalBacking for MeteredBacking<B> {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        self.inner.backing_kind()
    }

    fn can_export(&self) -> bool {
        self.inner.can_export()
    }

    fn residency(&self) -> styx_core::prelude::FrameResidency {
        self.inner.residency()
    }

    fn cpu_access(&self) -> styx_core::prelude::CpuAccess {
        self.inner.cpu_access()
    }

    #[cfg(unix)]
    fn export_backing(
        &self,
    ) -> Result<Option<styx_core::prelude::FrameBackingExport>, styx_core::prelude::FrameExportError>
    {
        self.inner.export_backing()
    }

    #[cfg(unix)]
    fn export_into(
        &self,
        out: &mut Vec<styx_core::prelude::FrameFdPlane>,
    ) -> Result<Option<styx_core::prelude::ExportedKind>, styx_core::prelude::FrameExportError>
    {
        self.inner.export_into(out)
    }
}

/// What the frame path pays for metrics per frame, measured over `iterations` frames: a frame
/// recorded before delivery (with 3A values and ISP times, as the PiSP path records them, and
/// its hops stamped), its buffer tracked, the frame received (its taken hop stamped and its hops
/// recorded) and the buffer released. For the overhead check in
/// `metrics_top --overhead`.
#[doc(hidden)]
pub fn frame_path_cost(iterations: u32) -> Duration {
    struct Empty;
    impl ExternalBacking for Empty {
        fn plane_data(&self, _: usize) -> Option<&[u8]> {
            None
        }
    }
    let m = CaptureMetrics::default();
    let res = styx_core::prelude::Resolution::new(1280, 800).expect("size");
    let format = styx_core::prelude::MediaFormat::new(
        styx_core::prelude::FourCc::NV12,
        res,
        styx_core::prelude::ColorSpace::Srgb,
    );
    let now = styx_core::prelude::TimestampClock::Monotonic
        .now_ns()
        .unwrap_or(1);
    let mut meta = FrameMeta::new(format, now).with_backend(BackendFrameMeta::Native(
        styx_core::prelude::NativeFrameMeta {
            exposure_ns: 10_000_000,
            analog_gain: 2.0,
            digital_gain: 1.0,
            ..Default::default()
        },
    ));
    meta.clock = Some(styx_core::prelude::TimestampClock::Monotonic);
    let aaa = AaaSample {
        ae_locked: true,
        awb_converged: true,
        colour_temperature: 4000.0,
        lux: 300.0,
        flicker_period: None,
        af: None,
    };
    let start = Instant::now();
    for i in 0..iterations {
        meta.timestamp += 8_333_333;
        if let Some(BackendFrameMeta::Native(n)) = &mut meta.backend {
            n.sequence = i;
        }
        m.isp_time(Duration::from_micros(2500), Duration::from_micros(3000));
        m.aaa(&aaa);
        meta.hops = styx_core::prelude::FrameHops::new();
        meta.hops.set(Hop::Dequeued, meta.timestamp + 5_000_000);
        meta.hops.set(Hop::IspDone, meta.timestamp + 8_000_000);
        stamp_queued(&mut meta);
        m.frame(std::hint::black_box(&meta));
        let buffer = m.track(Empty);
        m.taken(std::hint::black_box(&mut meta));
        drop(std::hint::black_box(buffer));
    }
    start.elapsed() / iterations.max(1)
}

/// What the hops alone cost per frame, over `iterations` frames, on every path a frame can
/// take: a native backend's hops (sensor, dequeue, ISP done) and the queue's stamped, taken by
/// a consumer and recorded in the capture's windows; then sent to another process (stamped and
/// recorded by the server, through the wire record), received and imported (stamped and
/// recorded by the consumer). For the overhead check in `metrics_top --overhead`.
#[doc(hidden)]
pub fn hop_path_cost(iterations: u32) -> Duration {
    use styx_core::prelude::{FrameHops, HopRecord};
    let capture = HopCounters::new();
    let server = HopCounters::new();
    let consumer = HopCounters::new();
    let res = styx_core::prelude::Resolution::new(1280, 800).expect("size");
    let format = styx_core::prelude::MediaFormat::new(
        styx_core::prelude::FourCc::NV12,
        res,
        styx_core::prelude::ColorSpace::Srgb,
    );
    let mut meta = FrameMeta::new(format, 1).with_backend(BackendFrameMeta::Native(
        styx_core::prelude::NativeFrameMeta::default(),
    ));
    meta.clock = Some(styx_core::prelude::TimestampClock::Monotonic);
    let start = Instant::now();
    for i in 0..iterations {
        let now = styx_core::prelude::CaptureInstant::now().as_nanos();
        meta.timestamp = now - 9_000_000;
        meta.hops = FrameHops::new();
        meta.hops.set(Hop::Dequeued, now - 3_000_000);
        meta.hops.set(Hop::IspDone, now - 1_000_000);
        if let Some(BackendFrameMeta::Native(n)) = &mut meta.backend {
            n.sequence = i;
        }
        stamp_queued(&mut meta);
        meta.hops.mark(Hop::Taken);
        capture.record(std::hint::black_box(&meta.hops));
        let mut sent = meta.hops;
        sent.mark(Hop::Sent);
        server.record(&sent);
        let record = std::hint::black_box(HopRecord::new(meta.sequence(), &sent));
        let mut received = record.hops();
        received.mark(Hop::Received);
        received.mark(Hop::Imported);
        consumer.record(std::hint::black_box(&received));
    }
    start.elapsed() / iterations.max(1)
}

impl Drop for Live {
    fn drop(&mut self) {
        let Ok(info) = self.info.lock() else {
            return;
        };
        if info.name.is_empty() {
            return;
        }
        let c = &self.counters;
        crate::trace::info!(
            camera = %info.name,
            capture = self.id,
            frames = c.rt.frames.get(),
            received = c.rt.received.get(),
            sensor_sequence_gaps = c.sequence_gaps.load(Relaxed),
            corrupted = c.rt.corrupted.get(),
            isp_skipped = c.rt.isp_skipped.get(),
            seconds = self.started.elapsed().as_secs_f64(),
            "capture closed"
        );
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

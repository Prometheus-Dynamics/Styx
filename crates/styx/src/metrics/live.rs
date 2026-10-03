//! Live per-capture metrics, recorded on the frame path with relaxed atomics only (no locks, no
//! allocation): counters, and fixed rings of the last [`WINDOW`] samples for frame intervals,
//! latencies, ISP times and buffer hold times. Snapshots (`super::camera`) read them on demand.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use styx_core::prelude::{BackendFrameMeta, ExternalBacking, FrameMeta};

use super::camera::Window;

/// Samples kept per ring: the window percentiles and the measured frame rate are over the last
/// `WINDOW` frames (about 4 s at 30 fps, 1 s at 120 fps).
pub const WINDOW: usize = 128;

/// The last [`WINDOW`] samples (nanoseconds) and the largest ever seen. Writers may race: a
/// reader can see a slot one sample old, which a percentile does not notice.
pub(crate) struct Ring {
    next: AtomicU64,
    max: AtomicU64,
    slots: [AtomicU64; WINDOW],
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(0),
            max: AtomicU64::new(0),
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Ring {
    #[inline]
    pub(crate) fn push(&self, ns: u64) {
        let i = self.next.fetch_add(1, Relaxed) as usize % WINDOW;
        self.slots[i].store(ns, Relaxed);
        // A read first: the maximum rarely changes, and a plain load costs less than an RMW.
        if ns > self.max.load(Relaxed) {
            self.max.fetch_max(ns, Relaxed);
        }
    }

    /// Samples ever pushed.
    pub(crate) fn count(&self) -> u64 {
        self.next.load(Relaxed)
    }

    /// The samples in the window, unordered.
    pub(crate) fn samples(&self) -> Vec<u64> {
        let n = (self.count() as usize).min(WINDOW);
        self.slots[..n].iter().map(|s| s.load(Relaxed)).collect()
    }

    pub(crate) fn window(&self) -> Window {
        Window::from_samples(self.samples(), self.count(), self.max.load(Relaxed))
    }
}

/// What a capture's 3A loop and sensor did for its latest frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AaaSample {
    pub ae_locked: bool,
    pub awb_converged: bool,
    pub colour_temperature: f64,
    pub lux: f64,
    /// Flicker period AE detected (`None`: none).
    pub flicker_period: Option<Duration>,
}

/// Latest sensor and 3A values, as bits of `f32`s and plain integers.
#[derive(Default)]
struct Aaa {
    /// 0 never reported; 1 searching; 2 converged.
    ae: AtomicU32,
    awb_converged: AtomicBool,
    colour_temperature: AtomicU32,
    lux: AtomicU32,
    flicker_us: AtomicU32,
    /// Exposure of the latest frame (0: not reported).
    exposure_ns: AtomicU64,
    analogue_gain: AtomicU32,
    digital_gain: AtomicU32,
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
        }
    }
}

/// Counters of a capture, added up across the backend captures a reconnecting capture ran.
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) frames: AtomicU64,
    pub(crate) received: AtomicU64,
    pub(crate) sequence_gaps: Arc<AtomicU64>,
    pub(crate) corrupted: AtomicU64,
    pub(crate) isp_skipped: AtomicU64,
    pub(crate) cpu_ns_retired: AtomicU64,
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
    /// Sequence numbers count sensor frames here (gaps are lost frames).
    track_sequence: AtomicBool,
    /// Gaps in the sequence are frames dropped as damaged (UVC), not lost by the sensor.
    gaps_are_corrupt: AtomicBool,
    last_sequence: AtomicU64,
    pending_isp_skips: AtomicU64,
    /// Frames are recorded where the backend produces them (else where they are received).
    producer: AtomicBool,
    last_timestamp: AtomicU64,
    pub(crate) intervals: Ring,
    /// Sensor timestamp to the frame entering the consumer queue.
    pub(crate) delivery: Ring,
    /// Sensor timestamp to the consumer taking the frame.
    pub(crate) receive: Ring,
    pub(crate) isp: Ring,
    pub(crate) processing: Ring,
    aaa: Aaa,
    aaa_reported: AtomicBool,
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
            track_sequence: AtomicBool::new(true),
            gaps_are_corrupt: AtomicBool::new(false),
            last_sequence: AtomicU64::new(u64::MAX),
            pending_isp_skips: AtomicU64::new(0),
            producer: AtomicBool::new(false),
            last_timestamp: AtomicU64::new(0),
            intervals: Ring::default(),
            delivery: Ring::default(),
            receive: Ring::default(),
            isp: Ring::default(),
            processing: Ring::default(),
            aaa: Aaa::default(),
            aaa_reported: AtomicBool::new(false),
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

fn f32_bits(v: f64) -> u32 {
    (v as f32).to_bits()
}

pub(crate) fn from_bits(v: u32) -> f64 {
    f64::from(f32::from_bits(v))
}

impl CaptureMetrics {
    /// For a backend that counts sequence gaps itself (into `gaps`, V4L2 and libcamera).
    #[cfg_attr(not(any(feature = "v4l2", feature = "libcamera")), allow(dead_code))]
    pub(crate) fn with_sequence_gaps(gaps: Arc<AtomicU64>) -> Self {
        let mut m = Self::default();
        let live = Arc::get_mut(&mut m.0).expect("new");
        live.counters.sequence_gaps = gaps;
        live.track_sequence = AtomicBool::new(false);
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
        l.counters.frames.fetch_add(1, Relaxed);
        if !l.producer.load(Relaxed) {
            l.producer.store(true, Relaxed);
        }
        self.timing(meta, &l.delivery);
        let (sequence, error) = match &meta.backend {
            Some(BackendFrameMeta::Native(n)) => {
                l.aaa.exposure_ns.store(n.exposure_ns, Relaxed);
                l.aaa.analogue_gain.store(n.analog_gain.to_bits(), Relaxed);
                l.aaa.digital_gain.store(n.digital_gain.to_bits(), Relaxed);
                (Some(n.sequence), n.error)
            }
            Some(BackendFrameMeta::Uvc(u)) => (Some(u.sequence), u.error),
            // V4L2_BUF_FLAG_ERROR
            Some(BackendFrameMeta::V4l2(v)) => (Some(v.sequence), v.flags & 0x40 != 0),
            _ => (None, false),
        };
        if error {
            l.counters.corrupted.fetch_add(1, Relaxed);
        }
        if let Some(sequence) = sequence
            && l.track_sequence.load(Relaxed)
        {
            let sequence = u64::from(sequence);
            let last = l.last_sequence.swap(sequence, Relaxed);
            if last != u64::MAX && sequence > last + 1 {
                let gap = sequence - last - 1;
                // Frames the ISP skipped are not the sensor's.
                let skipped = l.pending_isp_skips.swap(0, Relaxed).min(gap);
                self.gaps(gap - skipped);
            } else {
                l.pending_isp_skips.store(0, Relaxed);
            }
        }
    }

    fn gaps(&self, gaps: u64) {
        if gaps == 0 {
            return;
        }
        let l = &*self.0;
        let counter = if l.gaps_are_corrupt.load(Relaxed) {
            &l.counters.corrupted
        } else {
            &*l.counters.sequence_gaps
        };
        let before = counter.fetch_add(gaps, Relaxed);
        // Logged at 1, 2, 4, 8... lost frames: never once per frame.
        if (before + gaps).ilog2() != before.checked_ilog2().unwrap_or(u32::MAX) {
            tracing::info!(
                capture = l.id,
                lost = before + gaps,
                corrupt = l.gaps_are_corrupt.load(Relaxed),
                "frames missing from the sequence"
            );
        }
    }

    /// Frame interval (from sensor timestamps) and sensor-to-now latency into `latency`.
    #[inline]
    fn timing(&self, meta: &FrameMeta, latency: &Ring) {
        let ts = meta.timestamp;
        if ts == 0 {
            return;
        }
        let l = &*self.0;
        if let Some(now) = meta.clock.and_then(|c| c.now_ns())
            && now >= ts
        {
            latency.push(now - ts);
        }
        if std::ptr::eq(latency, &l.delivery) || !l.producer.load(Relaxed) {
            let last = l.last_timestamp.swap(ts, Relaxed);
            if last != 0 && ts > last {
                l.intervals.push(ts - last);
            }
        }
    }

    /// Record a frame the consumer took from the capture queue.
    #[inline]
    pub(crate) fn received(&self, meta: &FrameMeta) {
        self.0.counters.received.fetch_add(1, Relaxed);
        self.timing(meta, &self.0.receive);
    }

    /// The ISP dropped a frame (all its output buffers were held by consumers).
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn isp_skipped(&self) {
        self.0.counters.isp_skipped.fetch_add(1, Relaxed);
        self.0.pending_isp_skips.fetch_add(1, Relaxed);
    }

    /// Time the ISP spent on a frame (`isp`: the back end job or the software ISP pass) and the
    /// whole of its processing (`processing`: raw frame dequeued to outputs ready).
    #[inline]
    pub(crate) fn isp_time(&self, isp: Duration, processing: Duration) {
        self.0.isp.push(isp.as_nanos() as u64);
        self.0.processing.push(processing.as_nanos() as u64);
    }

    #[inline]
    pub(crate) fn aaa(&self, s: &AaaSample) {
        let a = &self.0.aaa;
        a.ae.store(if s.ae_locked { 2 } else { 1 }, Relaxed);
        a.awb_converged.store(s.awb_converged, Relaxed);
        a.colour_temperature
            .store(f32_bits(s.colour_temperature), Relaxed);
        a.lux.store(f32_bits(s.lux), Relaxed);
        let us = s.flicker_period.map_or(0, |p| p.as_micros() as u32);
        a.flicker_us.store(us, Relaxed);
        if !self.0.aaa_reported.load(Relaxed) {
            self.0.aaa_reported.store(true, Relaxed);
        }
    }

    pub(crate) fn aaa_state(&self) -> Option<super::camera::AaaState> {
        let a = &self.0.aaa;
        let exposure = a.exposure_ns.load(Relaxed);
        let loop_ran = self.0.aaa_reported.load(Relaxed);
        if exposure == 0 && !loop_ran {
            return None;
        }
        let flicker = a.flicker_us.load(Relaxed);
        Some(super::camera::AaaState {
            ae_state: match a.ae.load(Relaxed) {
                2 => Some("converged".into()),
                1 => Some("searching".into()),
                _ => None,
            },
            exposure_us: (exposure > 0).then(|| exposure as f64 / 1000.0),
            analogue_gain: (exposure > 0).then(|| from_bits(a.analogue_gain.load(Relaxed))),
            digital_gain: (exposure > 0).then(|| from_bits(a.digital_gain.load(Relaxed))),
            colour_temperature_k: loop_ran
                .then(|| from_bits(a.colour_temperature.load(Relaxed)))
                .filter(|k| *k > 0.0),
            lux: loop_ran.then(|| from_bits(a.lux.load(Relaxed))),
            awb_converged: loop_ran.then(|| a.awb_converged.load(Relaxed)),
            flicker_hz: (flicker > 0).then(|| 1e6 / f64::from(flicker)),
            af_state: None,
        })
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
        for (a, b) in [
            (&from.frames, &to.frames),
            (&from.received, &to.received),
            (&from.corrupted, &to.corrupted),
            (&from.isp_skipped, &to.isp_skipped),
        ] {
            b.fetch_add(a.load(Relaxed), Relaxed);
        }
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
        self.0.producer.load(Relaxed)
    }
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

    #[cfg(unix)]
    fn export_backing(
        &self,
    ) -> Result<Option<styx_core::prelude::FrameBackingExport>, styx_core::prelude::FrameExportError>
    {
        self.inner.export_backing()
    }
}

/// What the frame path pays for metrics per frame, measured over `iterations` frames: a frame
/// recorded before delivery (with 3A values and ISP times, as the PiSP path records them), its
/// buffer tracked, the frame received and the buffer released. For the overhead check in
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
    };
    let start = Instant::now();
    for i in 0..iterations {
        meta.timestamp += 8_333_333;
        if let Some(BackendFrameMeta::Native(n)) = &mut meta.backend {
            n.sequence = i;
        }
        m.isp_time(Duration::from_micros(2500), Duration::from_micros(3000));
        m.aaa(&aaa);
        m.frame(std::hint::black_box(&meta));
        let buffer = m.track(Empty);
        m.received(std::hint::black_box(&meta));
        drop(std::hint::black_box(buffer));
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
        tracing::info!(
            camera = %info.name,
            capture = self.id,
            frames = c.frames.load(Relaxed),
            received = c.received.load(Relaxed),
            sensor_sequence_gaps = c.sequence_gaps.load(Relaxed),
            corrupted = c.corrupted.load(Relaxed),
            isp_skipped = c.isp_skipped.load(Relaxed),
            seconds = self.started.elapsed().as_secs_f64(),
            "capture closed"
        );
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

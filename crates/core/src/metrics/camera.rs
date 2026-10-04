//! Metrics counters of a running camera, recorded on the frame path with relaxed atomics only
//! (no locks, no allocation, `no_std`): frames, drops by cause, fixed rings of the last
//! [`WINDOW`] samples for frame intervals, latencies and ISP times, the 3A loop's and AF's
//! state for the latest frame, and stills. 64-bit atomics come from `portable-atomic` (native
//! on 64-bit targets, a lock-based fallback on Cortex-M and 32-bit RISC-V).
//!
//! `styx-runtime` and the platforms only count (`styx_runtime::metrics` is this module);
//! reading is the platform's: `styx::metrics` snapshots these into its `CameraMetrics`
//! (percentiles, rates, CPU time, consumers), a firmware reads the counters directly.

use alloc::vec::Vec;
use core::time::Duration;

use portable_atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::sync::Counter;

/// Samples kept per ring: the window percentiles and the measured frame rate are over the last
/// `WINDOW` frames (about 4 s at 30 fps, 1 s at 120 fps).
pub const WINDOW: usize = 128;

/// The last [`WINDOW`] samples (nanoseconds) and the largest ever seen. Writers may race: a
/// reader can see a slot one sample old, which a percentile does not notice.
#[derive(Debug)]
pub struct Ring {
    next: AtomicU64,
    max: AtomicU64,
    slots: [AtomicU64; WINDOW],
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    /// An empty ring.
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            max: AtomicU64::new(0),
            slots: [const { AtomicU64::new(0) }; WINDOW],
        }
    }

    /// Records a sample.
    #[inline]
    pub fn push(&self, ns: u64) {
        let i = self.next.fetch_add(1, Relaxed) as usize % WINDOW;
        self.slots[i].store(ns, Relaxed);
        // A read first: the maximum rarely changes, and a plain load costs less than an RMW.
        if ns > self.max.load(Relaxed) {
            self.max.fetch_max(ns, Relaxed);
        }
    }

    /// Records a duration.
    #[inline]
    pub fn push_duration(&self, d: Duration) {
        self.push(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Samples ever pushed.
    pub fn count(&self) -> u64 {
        self.next.load(Relaxed)
    }

    /// The largest sample ever pushed.
    pub fn max_ever(&self) -> u64 {
        self.max.load(Relaxed)
    }

    /// Copies the samples in the window (unordered) into `out`; returns how many.
    pub fn copy_to(&self, out: &mut [u64; WINDOW]) -> usize {
        let n = (self.count() as usize).min(WINDOW);
        for (o, s) in out.iter_mut().zip(&self.slots[..n]) {
            *o = s.load(Relaxed);
        }
        n
    }

    /// The samples in the window, unordered.
    pub fn samples(&self) -> Vec<u64> {
        let n = (self.count() as usize).min(WINDOW);
        self.slots[..n].iter().map(|s| s.load(Relaxed)).collect()
    }

    /// The `q` quantile (0..=1) of the window, nearest rank, without allocating (`None`: empty).
    pub fn quantile(&self, q: f64) -> Option<u64> {
        let mut buf = [0u64; WINDOW];
        let n = self.copy_to(&mut buf);
        let s = &mut buf[..n];
        if s.is_empty() {
            return None;
        }
        s.sort_unstable();
        let i = ((s.len() - 1) as f64 * q.clamp(0.0, 1.0) + 0.5) as usize;
        s.get(i).copied()
    }
}

/// One frame as the camera produced it, for [`Counters::frame`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSample {
    /// The receiver's sequence number (`None`: the source has none).
    pub sequence: Option<u64>,
    /// Capture timestamp in nanoseconds on the platform clock (0: none).
    pub timestamp_ns: u64,
    /// The platform clock now, in nanoseconds (`None`: not known; no latency is recorded).
    pub now_ns: Option<u64>,
    /// The receiver flagged the frame as damaged.
    pub corrupt: bool,
    /// What produced the frame: exposure in nanoseconds, analogue and digital gain.
    pub exposure: Option<(u64, f32, f32)>,
}

/// AF states as the 3A loop reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfStateKind {
    /// Not scanning.
    Idle = 1,
    /// A scan is running.
    Scanning = 2,
    /// The last scan found focus.
    Focused = 3,
    /// The last scan failed.
    Failed = 4,
}

/// AF modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfModeKind {
    /// The application moves the lens.
    Manual = 1,
    /// Scans on a trigger.
    Auto = 2,
    /// Scans whenever the scene changes.
    Continuous = 3,
}

/// What AF did for one frame (cameras with a focus lens).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AfSample {
    /// The state.
    pub state: AfStateKind,
    /// The mode.
    pub mode: AfModeKind,
    /// The lens position AF commanded, in dioptres.
    pub lens_dioptres: Option<f64>,
    /// The lens had settled at its commanded position for the whole exposure (`None`: not
    /// reported for this frame).
    pub lens_settled: Option<bool>,
}

/// What a camera's 3A loop did for its latest frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AaaSample {
    /// AE locked.
    pub ae_locked: bool,
    /// AWB converged.
    pub awb_converged: bool,
    /// AWB's colour temperature (K).
    pub colour_temperature: f64,
    /// Scene brightness estimate.
    pub lux: f64,
    /// Flicker period AE detected (`None`: none).
    pub flicker_period: Option<Duration>,
    /// Autofocus, for cameras with a focus lens.
    pub af: Option<AfSample>,
}

/// The latest [`AfSample`] read back, and the scans counted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AfReading {
    /// The state.
    pub state: AfStateKind,
    /// The mode.
    pub mode: AfModeKind,
    /// The lens position commanded, in dioptres.
    pub lens_dioptres: Option<f64>,
    /// The lens had settled (`None`: not reported).
    pub lens_settled: Option<bool>,
    /// Scans started so far.
    pub scans: u64,
}

/// The sensor's and the 3A loop's state for the latest frame, as read back.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AaaReading {
    /// `Some(true)` converged, `Some(false)` searching (`None`: the loop never reported).
    pub ae_locked: Option<bool>,
    /// The latest frame's exposure, analogue and digital gain (`None`: not reported).
    pub exposure: Option<(Duration, f64, f64)>,
    /// Colour temperature (`None`: the loop never reported, or none).
    pub colour_temperature: Option<f64>,
    /// Lux (`None`: the loop never reported).
    pub lux: Option<f64>,
    /// AWB converged (`None`: the loop never reported).
    pub awb_converged: Option<bool>,
    /// The flicker period AE detected.
    pub flicker_period: Option<Duration>,
    /// Autofocus (`None`: never reported).
    pub af: Option<AfReading>,
}

fn f32_bits(v: f64) -> u32 {
    (v as f32).to_bits()
}

fn from_bits(v: u32) -> f64 {
    f64::from(f32::from_bits(v))
}

/// Latest sensor, 3A and AF values, as bits of `f32`s and plain integers.
#[derive(Debug)]
pub struct AaaCounters {
    /// 0 never reported; 1 searching; 2 converged.
    ae: AtomicU8,
    awb_converged: AtomicBool,
    colour_temperature: AtomicU32,
    lux: AtomicU32,
    flicker_us: AtomicU32,
    reported: AtomicBool,
    /// Exposure of the latest frame (0: not reported).
    exposure_ns: AtomicU64,
    analogue_gain: AtomicU32,
    digital_gain: AtomicU32,
    /// 0: never reported; else an [`AfStateKind`].
    af_state: AtomicU8,
    af_mode: AtomicU8,
    /// `f32` bits; NaN without a position.
    lens: AtomicU32,
    /// 0 unknown, 1 moving, 2 settled.
    settled: AtomicU8,
    scans: AtomicU64,
}

impl Default for AaaCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl AaaCounters {
    /// Nothing reported yet.
    pub const fn new() -> Self {
        Self {
            ae: AtomicU8::new(0),
            awb_converged: AtomicBool::new(false),
            colour_temperature: AtomicU32::new(0),
            lux: AtomicU32::new(0),
            flicker_us: AtomicU32::new(0),
            reported: AtomicBool::new(false),
            exposure_ns: AtomicU64::new(0),
            analogue_gain: AtomicU32::new(0),
            digital_gain: AtomicU32::new(0),
            af_state: AtomicU8::new(0),
            af_mode: AtomicU8::new(0),
            lens: AtomicU32::new(0x7fc0_0000), // f32 NaN
            settled: AtomicU8::new(0),
            scans: AtomicU64::new(0),
        }
    }

    /// What produced the latest frame.
    #[inline]
    pub fn exposure(&self, exposure_ns: u64, analogue_gain: f32, digital_gain: f32) {
        self.exposure_ns.store(exposure_ns, Relaxed);
        self.analogue_gain.store(analogue_gain.to_bits(), Relaxed);
        self.digital_gain.store(digital_gain.to_bits(), Relaxed);
    }

    /// What the 3A loop made of the latest frame.
    #[inline]
    pub fn record(&self, s: &AaaSample) {
        self.ae.store(if s.ae_locked { 2 } else { 1 }, Relaxed);
        self.awb_converged.store(s.awb_converged, Relaxed);
        self.colour_temperature
            .store(f32_bits(s.colour_temperature), Relaxed);
        self.lux.store(f32_bits(s.lux), Relaxed);
        let us = s
            .flicker_period
            .map_or(0, |p| u32::try_from(p.as_micros()).unwrap_or(u32::MAX));
        self.flicker_us.store(us, Relaxed);
        if let Some(af) = &s.af {
            self.record_af(af);
        }
        if !self.reported.load(Relaxed) {
            self.reported.store(true, Relaxed);
        }
    }

    #[inline]
    fn record_af(&self, s: &AfSample) {
        let previous = self.af_state.swap(s.state as u8, Relaxed);
        // A scan starts when AF enters scanning (a continuous scan, or each trigger).
        if s.state == AfStateKind::Scanning && previous != AfStateKind::Scanning as u8 {
            self.scans.fetch_add(1, Relaxed);
        }
        self.af_mode.store(s.mode as u8, Relaxed);
        let lens = s.lens_dioptres.map_or(f32::NAN, |d| d as f32);
        self.lens.store(lens.to_bits(), Relaxed);
        let settled = s.lens_settled.map_or(0, |s| 1 + u8::from(s));
        self.settled.store(settled, Relaxed);
    }

    /// The latest values (`None`: neither a frame's exposure nor the loop was reported).
    pub fn read(&self) -> Option<AaaReading> {
        let exposure = self.exposure_ns.load(Relaxed);
        let ran = self.reported.load(Relaxed);
        if exposure == 0 && !ran {
            return None;
        }
        let flicker = self.flicker_us.load(Relaxed);
        Some(AaaReading {
            ae_locked: match self.ae.load(Relaxed) {
                2 => Some(true),
                1 => Some(false),
                _ => None,
            },
            exposure: (exposure > 0).then(|| {
                (
                    Duration::from_nanos(exposure),
                    from_bits(self.analogue_gain.load(Relaxed)),
                    from_bits(self.digital_gain.load(Relaxed)),
                )
            }),
            colour_temperature: ran
                .then(|| from_bits(self.colour_temperature.load(Relaxed)))
                .filter(|k| *k > 0.0),
            lux: ran.then(|| from_bits(self.lux.load(Relaxed))),
            awb_converged: ran.then(|| self.awb_converged.load(Relaxed)),
            flicker_period: (flicker > 0).then(|| Duration::from_micros(u64::from(flicker))),
            af: self.read_af(),
        })
    }

    fn read_af(&self) -> Option<AfReading> {
        let state = match self.af_state.load(Relaxed) {
            0 => return None,
            1 => AfStateKind::Idle,
            2 => AfStateKind::Scanning,
            3 => AfStateKind::Focused,
            _ => AfStateKind::Failed,
        };
        let mode = match self.af_mode.load(Relaxed) {
            1 => AfModeKind::Manual,
            2 => AfModeKind::Auto,
            _ => AfModeKind::Continuous,
        };
        let lens = f32::from_bits(self.lens.load(Relaxed));
        Some(AfReading {
            state,
            mode,
            lens_dioptres: (!lens.is_nan()).then_some(f64::from(lens)),
            lens_settled: match self.settled.load(Relaxed) {
                0 => None,
                s => Some(s == 2),
            },
            scans: self.scans.load(Relaxed),
        })
    }
}

/// Stills taken from a camera.
#[derive(Debug, Default)]
pub struct StillCounters {
    /// Requests finished (taken or failed).
    pub requests: Counter,
    /// Requests that failed.
    pub failed: Counter,
    /// Shots taken.
    pub shots: Counter,
    /// Shots on the frame their exposure was to land on, with it (brackets, fixed exposures).
    pub landed: Counter,
    /// Shots that were not.
    pub missed: Counter,
    /// Request to ready.
    pub latency: Ring,
}

impl StillCounters {
    /// A still request finished: `Some((latency, shots, landed))`, or `None` when it failed.
    pub fn record(&self, done: Option<(Duration, u64, u64)>) {
        self.requests.incr();
        match done {
            Some((latency, shots, landed)) => {
                self.shots.add(shots);
                self.landed.add(landed);
                self.missed.add(shots.saturating_sub(landed));
                self.latency.push_duration(latency);
            }
            None => self.failed.incr(),
        }
    }
}

/// A running camera's counters. See the [module documentation](crate::metrics).
#[derive(Debug)]
pub struct Counters {
    /// Frames produced (recorded with [`Self::frame`]).
    pub frames: Counter,
    /// Frames the consumer took ([`Self::received`]).
    pub received: Counter,
    /// Frames the receiver flagged as damaged (and, for sources whose sequence gaps are
    /// damaged frames, those gaps: [`Self::set_gaps_are_corrupt`]).
    pub corrupted: Counter,
    /// Frames the ISP dropped (its output buffers were all held).
    pub isp_skipped: Counter,
    /// Frames missing from the sequence that the ISP did not skip (lost by the sensor or the
    /// receiver), when the caller adds [`Self::frame`]'s result here.
    pub sequence_gaps: Counter,
    /// Frame intervals from the capture timestamps.
    pub intervals: Ring,
    /// Capture timestamp to the frame being produced (entering the consumer queue).
    pub delivery: Ring,
    /// Capture timestamp to the consumer taking the frame.
    pub receive: Ring,
    /// The ISP's time per frame (a back end job, a software ISP pass).
    pub isp: Ring,
    /// Raw frame dequeued to outputs ready (statistics, algorithms, ISP).
    pub processing: Ring,
    /// The sensor's and the 3A loop's latest state.
    pub aaa: AaaCounters,
    /// Stills.
    pub stills: StillCounters,
    /// Sequence numbers count sensor frames (gaps are lost frames).
    track_sequence: AtomicBool,
    /// Gaps are frames the source dropped as damaged (UVC), not lost by the sensor.
    gaps_are_corrupt: AtomicBool,
    last_sequence: AtomicU64,
    pending_isp_skips: AtomicU64,
    /// Frames are recorded where they are produced (else where they are received).
    producer: AtomicBool,
    last_timestamp: AtomicU64,
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

impl Counters {
    /// Counters at zero.
    pub const fn new() -> Self {
        Self {
            frames: Counter::new(),
            received: Counter::new(),
            corrupted: Counter::new(),
            isp_skipped: Counter::new(),
            sequence_gaps: Counter::new(),
            intervals: Ring::new(),
            delivery: Ring::new(),
            receive: Ring::new(),
            isp: Ring::new(),
            processing: Ring::new(),
            aaa: AaaCounters::new(),
            stills: StillCounters {
                requests: Counter::new(),
                failed: Counter::new(),
                shots: Counter::new(),
                landed: Counter::new(),
                missed: Counter::new(),
                latency: Ring::new(),
            },
            track_sequence: AtomicBool::new(true),
            gaps_are_corrupt: AtomicBool::new(false),
            last_sequence: AtomicU64::new(u64::MAX),
            pending_isp_skips: AtomicU64::new(0),
            producer: AtomicBool::new(false),
            last_timestamp: AtomicU64::new(0),
        }
    }

    /// Whether [`Self::frame`] tracks sequence gaps (off: the source counts its own).
    pub fn set_track_sequence(&self, on: bool) {
        self.track_sequence.store(on, Relaxed);
    }

    /// Sequence gaps are frames the source dropped as damaged: counted as corrupted.
    pub fn set_gaps_are_corrupt(&self, on: bool) {
        self.gaps_are_corrupt.store(on, Relaxed);
    }

    /// Whether gaps count as corrupted frames.
    pub fn gaps_are_corrupt(&self) -> bool {
        self.gaps_are_corrupt.load(Relaxed)
    }

    /// Records a frame about to be delivered: the count, its interval and capture-to-now
    /// latency, damage, what produced it, and sequence gaps. Returns the frames missing before
    /// it that the ISP did not skip, for the caller to count where it keeps them
    /// ([`Self::sequence_gaps`], or a counter shared with the source); with
    /// [`Self::set_gaps_are_corrupt`] they are added to [`Self::corrupted`] here and 0 is
    /// returned.
    #[inline]
    pub fn frame(&self, f: &FrameSample) -> u64 {
        self.frames.incr();
        if !self.producer.load(Relaxed) {
            self.producer.store(true, Relaxed);
        }
        self.timing(f.timestamp_ns, f.now_ns, &self.delivery, true);
        if let Some((ns, ag, dg)) = f.exposure {
            self.aaa.exposure(ns, ag, dg);
        }
        if f.corrupt {
            self.corrupted.incr();
        }
        let Some(sequence) = f.sequence else {
            return 0;
        };
        if !self.track_sequence.load(Relaxed) {
            return 0;
        }
        let last = self.last_sequence.swap(sequence, Relaxed);
        if last != u64::MAX && sequence > last + 1 {
            let gap = sequence - last - 1;
            // Frames the ISP skipped are not the sensor's.
            let skipped = self.pending_isp_skips.swap(0, Relaxed).min(gap);
            let lost = gap - skipped;
            if lost > 0 && self.gaps_are_corrupt.load(Relaxed) {
                self.corrupted.add(lost);
                return 0;
            }
            lost
        } else {
            self.pending_isp_skips.store(0, Relaxed);
            0
        }
    }

    /// Records a frame the consumer took.
    #[inline]
    pub fn received(&self, timestamp_ns: u64, now_ns: Option<u64>) {
        self.received.incr();
        self.timing(timestamp_ns, now_ns, &self.receive, false);
    }

    /// Frames are recorded where they are produced ([`Self::frame`] was called).
    pub fn has_producer(&self) -> bool {
        self.producer.load(Relaxed)
    }

    /// The ISP dropped a frame (all its output buffers were held by consumers).
    #[inline]
    pub fn isp_skipped(&self) {
        self.isp_skipped.incr();
        self.pending_isp_skips.fetch_add(1, Relaxed);
    }

    /// Time the ISP spent on a frame and the whole of its processing.
    #[inline]
    pub fn isp_time(&self, isp: Duration, processing: Duration) {
        self.isp.push_duration(isp);
        self.processing.push_duration(processing);
    }

    /// Frame interval (from capture timestamps) and capture-to-now latency into `latency`.
    #[inline]
    fn timing(&self, ts: u64, now: Option<u64>, latency: &Ring, produced: bool) {
        if ts == 0 {
            return;
        }
        if let Some(now) = now
            && now >= ts
        {
            latency.push(now - ts);
        }
        if produced || !self.producer.load(Relaxed) {
            let last = self.last_timestamp.swap(ts, Relaxed);
            if last != 0 && ts > last {
                self.intervals.push(ts - last);
            }
        }
    }

    /// The frame rate over the window of intervals (`None`: no interval yet).
    pub fn measured_fps(&self) -> Option<f64> {
        let mut buf = [0u64; WINDOW];
        let n = self.intervals.copy_to(&mut buf);
        (n > 0).then(|| {
            let mean = buf[..n].iter().sum::<u64>() as f64 / n as f64;
            1e9 / mean.max(1.0)
        })
    }

    /// Adds `previous`'s frame and drop counters to these (a capture replacing another).
    pub fn absorb(&self, previous: &Counters) {
        for (a, b) in [
            (&previous.frames, &self.frames),
            (&previous.received, &self.received),
            (&previous.corrupted, &self.corrupted),
            (&previous.isp_skipped, &self.isp_skipped),
            (&previous.sequence_gaps, &self.sequence_gaps),
        ] {
            b.add(a.get());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64, ms: u64, corrupt: bool) -> FrameSample {
        FrameSample {
            sequence: Some(sequence),
            timestamp_ns: ms * 1_000_000,
            now_ns: Some(ms * 1_000_000 + 8_000_000),
            corrupt,
            exposure: Some((10_000_000, 2.0, 1.0)),
        }
    }

    #[test]
    fn drops_are_told_apart_by_cause() {
        let c = Counters::new();
        // Frames 0, 1, then 3 (one lost by the sensor), the ISP skips one (4), then 5.
        for f in [
            frame(0, 1000, false),
            frame(1, 1033, false),
            frame(3, 1100, false),
        ] {
            let lost = c.frame(&f);
            c.sequence_gaps.add(lost);
        }
        c.isp_skipped();
        assert_eq!(c.frame(&frame(5, 1166, true)), 0);
        assert_eq!(c.frames.get(), 4);
        assert_eq!(c.sequence_gaps.get(), 1);
        assert_eq!((c.isp_skipped.get(), c.corrupted.get()), (1, 1));
        assert!((c.measured_fps().unwrap() - 3.0 / 0.166).abs() < 0.1);
        assert_eq!(c.delivery.count(), 4);
        assert_eq!(c.delivery.quantile(0.5), Some(8_000_000));
        let a = c.aaa.read().unwrap();
        assert_eq!(a.ae_locked, None);
        assert_eq!(a.exposure, Some((Duration::from_millis(10), 2.0, 1.0)));
    }

    #[test]
    fn gaps_can_count_as_corrupt_frames() {
        let c = Counters::new();
        c.set_gaps_are_corrupt(true);
        c.frame(&frame(0, 1000, false));
        assert_eq!(c.frame(&frame(3, 1100, false)), 0);
        assert_eq!(c.corrupted.get(), 2);
    }

    #[test]
    fn rings_keep_the_window_and_the_maximum() {
        let r = Ring::new();
        for ms in 1..=200u64 {
            r.push(ms * 1_000_000);
        }
        assert_eq!((r.count(), r.max_ever()), (200, 200_000_000));
        assert_eq!(r.samples().len(), WINDOW);
        // The window holds 73..=200 ms.
        let p50 = r.quantile(0.5).unwrap();
        assert!((136_000_000..=137_000_000).contains(&p50), "{p50}");
        assert_eq!(Ring::new().quantile(0.5), None);
    }

    #[test]
    fn af_scans_are_counted_and_stills_split_into_landed_and_missed() {
        let c = Counters::new();
        let sample = |state| AaaSample {
            ae_locked: true,
            af: Some(AfSample {
                state,
                mode: AfModeKind::Continuous,
                lens_dioptres: Some(2.0),
                lens_settled: Some(true),
            }),
            ..AaaSample::default()
        };
        for s in [
            AfStateKind::Scanning,
            AfStateKind::Scanning,
            AfStateKind::Focused,
            AfStateKind::Scanning,
        ] {
            c.aaa.record(&sample(s));
        }
        let af = c.aaa.read().unwrap().af.unwrap();
        assert_eq!((af.state, af.scans), (AfStateKind::Scanning, 2));
        assert_eq!(af.lens_dioptres, Some(2.0));
        c.stills.record(Some((Duration::from_millis(80), 3, 2)));
        c.stills.record(None);
        let s = &c.stills;
        assert_eq!(
            (
                s.requests.get(),
                s.failed.get(),
                s.shots.get(),
                s.landed.get(),
                s.missed.get()
            ),
            (2, 1, 3, 2, 1)
        );
    }
}

//! Per-frame hop timestamps: when a frame passed each step between the sensor and a consumer
//! ([`Hop`]), carried in its metadata ([`FrameMeta::hops`](super::FrameMeta::hops)) from the
//! backend through queues, processes and the frame socket, so the time between any two steps
//! (and where a frame was copied) can be measured per frame and joined with other tools' records.
//!
//! Every hop is `CLOCK_MONOTONIC` nanoseconds (with `std`: the [`Instant`](std::time::Instant)
//! timeline, which is `CLOCK_MONOTONIC` on Linux; without `std` the platform clock
//! [`set_platform_clock`](super::set_platform_clock) sets). The join key across tools is the
//! frame's sequence number with its sensor timestamp ([`HopRecord`]).
//!
//! The record is fixed-size and `Copy` (no allocation); hops a frame did not pass, or a backend
//! does not know, stay `None`. Without the `path-metrics` feature (on with `std`)
//! [`FrameHops`] is empty: setting a hop does nothing and every hop reads `None`, so a
//! microcontroller build pays nothing for it.

#[cfg(feature = "path-metrics")]
use core::num::NonZeroU64;

/// A step between the sensor and a consumer, in the order a frame passes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Hop {
    /// The sensor timestamp the receiver reports, converted to `CLOCK_MONOTONIC`: the start of
    /// the frame (first line received) on `rp1-cfe` and `unicam` (native captures), what the
    /// driver reports for V4L2 (`uvcvideo`: the frame's first payload, or the PTS-based
    /// capture time), libcamera's `SensorTimestamp` (start of exposure of the first line).
    Sensor = 0,
    /// Styx took the frame from the kernel or driver: `VIDIOC_DQBUF` returned (native, V4L2),
    /// the request completed (libcamera), the frame was assembled (UVC).
    Dequeued = 1,
    /// The ISP finished the frame: the PiSP back end job (and its extra passes) done, the
    /// software or GPU ISP pass done.
    IspDone = 2,
    /// The frame entered the capture's consumer queue (its buffer leased to consumers).
    Queued = 3,
    /// A consumer in the capturing process took it from the queue (`recv`).
    Taken = 4,
    /// A frame socket or camera service sent it to another process (`sendmsg` returned).
    Sent = 5,
    /// The consumer process received the message and its descriptors.
    Received = 6,
    /// The consumer imported it: a frame built over the descriptors (and, for camera service
    /// clients, the buffer's mapping found or made), ready to read.
    Imported = 7,
}

/// Hops in a [`FrameHops`].
pub const HOP_COUNT: usize = 8;

impl Hop {
    /// Every hop, in order.
    pub const ALL: [Hop; HOP_COUNT] = [
        Hop::Sensor,
        Hop::Dequeued,
        Hop::IspDone,
        Hop::Queued,
        Hop::Taken,
        Hop::Sent,
        Hop::Received,
        Hop::Imported,
    ];

    /// Snake-case name, as JSON keys and metric labels use it.
    pub const fn name(self) -> &'static str {
        match self {
            Hop::Sensor => "sensor",
            Hop::Dequeued => "dequeued",
            Hop::IspDone => "isp_done",
            Hop::Queued => "queued",
            Hop::Taken => "taken",
            Hop::Sent => "sent",
            Hop::Received => "received",
            Hop::Imported => "imported",
        }
    }

    /// The hop named `name` ([`Hop::name`]).
    pub fn from_name(name: &str) -> Option<Hop> {
        Hop::ALL.into_iter().find(|h| h.name() == name)
    }

    /// Index in [`Hop::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// When a frame passed each [`Hop`] (`CLOCK_MONOTONIC` ns), and the copies made of its pixels
/// on the way. See the [module documentation](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FrameHops {
    #[cfg(feature = "path-metrics")]
    at: [Option<NonZeroU64>; HOP_COUNT],
    /// Copies of the frame's pixels made to produce it (conversions, crops into new memory,
    /// memfd copies, software ISP output copies).
    #[cfg(feature = "path-metrics")]
    copies: u32,
    #[cfg(feature = "path-metrics")]
    copied_bytes: u64,
    /// The receiver's sequence number, carried with the hops where the backend's metadata does
    /// not travel (another process): the join key with [`Hop::Sensor`].
    #[cfg(feature = "path-metrics")]
    sequence: Option<u32>,
}

impl FrameHops {
    /// Whether hops are recorded in this build (the `path-metrics` feature).
    pub const ENABLED: bool = cfg!(feature = "path-metrics");

    /// No hops, no copies.
    pub const fn new() -> Self {
        Self {
            #[cfg(feature = "path-metrics")]
            at: [None; HOP_COUNT],
            #[cfg(feature = "path-metrics")]
            copies: 0,
            #[cfg(feature = "path-metrics")]
            copied_bytes: 0,
            #[cfg(feature = "path-metrics")]
            sequence: None,
        }
    }

    /// The sequence number carried with the hops (see [`FrameMeta::sequence`](super::FrameMeta::sequence),
    /// which falls back to it).
    pub fn sequence(&self) -> Option<u32> {
        #[cfg(feature = "path-metrics")]
        {
            self.sequence
        }
        #[cfg(not(feature = "path-metrics"))]
        None
    }

    /// Carries `sequence` with the hops.
    pub fn set_sequence(&mut self, sequence: Option<u32>) {
        #[cfg(feature = "path-metrics")]
        {
            self.sequence = sequence;
        }
        #[cfg(not(feature = "path-metrics"))]
        let _ = sequence;
    }

    /// When the frame passed `hop`.
    #[inline]
    pub fn get(&self, hop: Hop) -> Option<u64> {
        #[cfg(feature = "path-metrics")]
        {
            self.at[hop.index()].map(NonZeroU64::get)
        }
        #[cfg(not(feature = "path-metrics"))]
        {
            let _ = hop;
            None
        }
    }

    /// Records that the frame passed `hop` at `ns` (0: unknown, clears it).
    #[inline]
    pub fn set(&mut self, hop: Hop, ns: u64) {
        #[cfg(feature = "path-metrics")]
        {
            self.at[hop.index()] = NonZeroU64::new(ns);
        }
        #[cfg(not(feature = "path-metrics"))]
        let _ = (hop, ns);
    }

    /// Records `hop` at `ns` unless it is already set.
    #[inline]
    pub fn set_if_unset(&mut self, hop: Hop, ns: u64) {
        if self.get(hop).is_none() {
            self.set(hop, ns);
        }
    }

    /// Records that the frame passes `hop` now (nothing without a clock: no `std` and no
    /// [`set_platform_clock`](super::set_platform_clock)).
    #[inline]
    pub fn mark(&mut self, hop: Hop) {
        #[cfg(feature = "path-metrics")]
        if let Some(now) = super::CaptureInstant::try_now() {
            self.set(hop, now.as_nanos());
        }
        #[cfg(not(feature = "path-metrics"))]
        let _ = hop;
    }

    /// No hop recorded and no copy counted.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none() && self.copies() == 0
    }

    /// The recorded hops, in order.
    pub fn iter(&self) -> impl Iterator<Item = (Hop, u64)> + '_ {
        Hop::ALL
            .into_iter()
            .filter_map(move |h| self.get(h).map(|ns| (h, ns)))
    }

    /// Each recorded hop after the first with the time since the hop recorded before it
    /// (`None` when the clock went backwards between them, e.g. a sensor timestamp converted
    /// from another clock).
    pub fn deltas(&self) -> impl Iterator<Item = (Hop, Hop, Option<u64>)> + '_ {
        let mut previous: Option<(Hop, u64)> = None;
        self.iter().filter_map(move |(hop, ns)| {
            let out = previous.map(|(from, at)| (from, hop, ns.checked_sub(at)));
            previous = Some((hop, ns));
            out
        })
    }

    /// The first and the last recorded hop.
    pub fn span(&self) -> Option<((Hop, u64), (Hop, u64))> {
        let first = self.iter().next()?;
        let last = self.iter().last()?;
        Some((first, last))
    }

    /// Counts a copy of `bytes` bytes of this frame's pixels.
    #[inline]
    pub fn copied(&mut self, bytes: usize) {
        #[cfg(feature = "path-metrics")]
        {
            self.copies = self.copies.saturating_add(1);
            self.copied_bytes = self.copied_bytes.saturating_add(bytes as u64);
        }
        #[cfg(not(feature = "path-metrics"))]
        let _ = bytes;
    }

    /// Copies counted.
    pub fn copies(&self) -> u32 {
        #[cfg(feature = "path-metrics")]
        {
            self.copies
        }
        #[cfg(not(feature = "path-metrics"))]
        0
    }

    /// Bytes copied.
    pub fn copied_bytes(&self) -> u64 {
        #[cfg(feature = "path-metrics")]
        {
            self.copied_bytes
        }
        #[cfg(not(feature = "path-metrics"))]
        0
    }

    /// Sets the copy counts (a record carried across a process boundary).
    pub fn set_copies(&mut self, copies: u32, bytes: u64) {
        #[cfg(feature = "path-metrics")]
        {
            self.copies = copies;
            self.copied_bytes = bytes;
        }
        #[cfg(not(feature = "path-metrics"))]
        let _ = (copies, bytes);
    }

    /// Keeps the hops set here and fills the others from `earlier` (the record of the frame a
    /// stage consumed); copies add up.
    pub fn merge_from(&mut self, earlier: &FrameHops) {
        for (hop, ns) in earlier.iter() {
            self.set_if_unset(hop, ns);
        }
        #[cfg(feature = "path-metrics")]
        {
            self.copies = self.copies.saturating_add(earlier.copies);
            self.copied_bytes = self.copied_bytes.saturating_add(earlier.copied_bytes);
            self.sequence = self.sequence.or(earlier.sequence);
        }
    }
}

/// A frame's hops as exported: the join key (sequence, sensor timestamp) and every hop, for
/// stitching with other tools' per-frame records. All times `CLOCK_MONOTONIC` nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HopRecord {
    /// The receiver's frame sequence number (`None`: the source has none).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub sequence: Option<u32>,
    /// The sensor timestamp ([`Hop::Sensor`]).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub sensor: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub dequeued: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub isp_done: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub queued: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub taken: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub sent: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub received: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub imported: Option<u64>,
    /// Copies of the pixels made on the way, and their bytes.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_zero"))]
    pub copies: u32,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_zero_u64"))]
    pub copied_bytes: u64,
}

#[cfg(feature = "serde")]
fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[cfg(feature = "serde")]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl HopRecord {
    /// The record of a frame with `sequence` and `hops`.
    pub fn new(sequence: Option<u32>, hops: &FrameHops) -> Self {
        let g = |h| hops.get(h);
        Self {
            sequence: sequence.or(hops.sequence()),
            sensor: g(Hop::Sensor),
            dequeued: g(Hop::Dequeued),
            isp_done: g(Hop::IspDone),
            queued: g(Hop::Queued),
            taken: g(Hop::Taken),
            sent: g(Hop::Sent),
            received: g(Hop::Received),
            imported: g(Hop::Imported),
            copies: hops.copies(),
            copied_bytes: hops.copied_bytes(),
        }
    }

    /// The hops as a [`FrameHops`].
    pub fn hops(&self) -> FrameHops {
        let mut hops = FrameHops::new();
        for (hop, ns) in Hop::ALL.into_iter().zip(self.times()) {
            if let Some(ns) = ns {
                hops.set(hop, ns);
            }
        }
        hops.set_copies(self.copies, self.copied_bytes);
        hops.set_sequence(self.sequence);
        hops
    }

    /// Each hop's time, in [`Hop::ALL`] order.
    pub fn times(&self) -> [Option<u64>; HOP_COUNT] {
        [
            self.sensor,
            self.dequeued,
            self.isp_done,
            self.queued,
            self.taken,
            self.sent,
            self.received,
            self.imported,
        ]
    }

    /// No hop and no copy.
    pub fn is_empty(&self) -> bool {
        self.times().iter().all(Option::is_none) && self.copies == 0
    }
}

#[cfg(all(test, feature = "path-metrics"))]
mod tests {
    use super::*;

    #[test]
    fn hops_record_deltas_and_merge() {
        let mut h = FrameHops::new();
        assert!(h.is_empty());
        h.set(Hop::Sensor, 1_000);
        h.set(Hop::Dequeued, 9_000);
        h.set(Hop::Queued, 12_000);
        h.set(Hop::Sent, 11_000); // clock went backwards: no delta
        let d: Vec<_> = h.deltas().collect();
        assert_eq!(
            d,
            [
                (Hop::Sensor, Hop::Dequeued, Some(8_000)),
                (Hop::Dequeued, Hop::Queued, Some(3_000)),
                (Hop::Queued, Hop::Sent, None),
            ]
        );
        let mut later = FrameHops::new();
        later.set(Hop::Dequeued, 10_000);
        later.copied(100);
        h.copied(50);
        later.merge_from(&h);
        assert_eq!(later.get(Hop::Dequeued), Some(10_000));
        assert_eq!(later.get(Hop::Sensor), Some(1_000));
        assert_eq!((later.copies(), later.copied_bytes()), (2, 150));
        let r = HopRecord::new(Some(7), &later);
        later.set_sequence(Some(7));
        assert_eq!(r.hops(), later);
        assert_eq!(r.sequence, Some(7));
        assert_eq!(Hop::from_name("isp_done"), Some(Hop::IspDone));
        assert_eq!(core::mem::size_of::<FrameHops>(), 88);
    }

    #[test]
    fn mark_uses_the_monotonic_clock() {
        let mut h = FrameHops::new();
        h.mark(Hop::Taken);
        let t = h.get(Hop::Taken).unwrap();
        #[cfg(target_os = "linux")]
        {
            let mono = super::super::TimestampClock::Monotonic.now_ns().unwrap();
            assert!(mono.abs_diff(t) < 50_000_000);
        }
        let _ = t;
    }
}

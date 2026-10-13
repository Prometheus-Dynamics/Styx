use core::{fmt, time::Duration};

use crate::format::MediaFormat;

pub use super::hops::{FrameHops, HOP_COUNT, Hop, HopRecord};

pub use super::clock::{
    CaptureInstant, ClockConversion, ClockSource, PlatformClock, TimestampClock, set_platform_clock,
};

/// Runtime-visible residency for a frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameResidency {
    HostOwned,
    HostExternal,
    Dmabuf,
    GpuTexture,
    CompressedPacket,
}

impl fmt::Display for FrameResidency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostOwned => write!(f, "host_owned"),
            Self::HostExternal => write!(f, "host_external"),
            Self::Dmabuf => write!(f, "dmabuf"),
            Self::GpuTexture => write!(f, "gpu_texture"),
            Self::CompressedPacket => write!(f, "compressed_packet"),
        }
    }
}

/// Mutability contract visible to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FrameMutability {
    #[default]
    Mutable,
    ReadOnly,
}

/// Reason a frame changed residency or had to be materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidencyTransitionReason {
    Capture,
    Decode,
    Encode,
    FrameHook,
    ImageHook,
    PackedTransform,
    ImageMaterialize,
    FileReplay,
    NetcamIngress,
    BackendFallbackCopy,
    Unknown,
}

impl fmt::Display for ResidencyTransitionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture => write!(f, "capture"),
            Self::Decode => write!(f, "decode"),
            Self::Encode => write!(f, "encode"),
            Self::FrameHook => write!(f, "frame_hook"),
            Self::ImageHook => write!(f, "image_hook"),
            Self::PackedTransform => write!(f, "packed_transform"),
            Self::ImageMaterialize => write!(f, "image_materialize"),
            Self::FileReplay => write!(f, "file_replay"),
            Self::NetcamIngress => write!(f, "netcam_ingress"),
            Self::BackendFallbackCopy => write!(f, "backend_fallback_copy"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Diagnostic record describing the last residency transition on a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResidencyTransition {
    pub from: FrameResidency,
    pub to: FrameResidency,
    pub reason: ResidencyTransitionReason,
    pub copied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendFrameMeta {
    V4l2(V4l2FrameMeta),
    Libcamera(LibcameraFrameMeta),
    /// A sensor driven by Styx itself (the native stack).
    Native(NativeFrameMeta),
    /// A UVC camera driven from userspace over usbfs (`styx-uvc`).
    Uvc(UvcFrameMeta),
}

impl BackendFrameMeta {
    pub fn as_v4l2(&self) -> Option<&V4l2FrameMeta> {
        match self {
            Self::V4l2(meta) => Some(meta),
            _ => None,
        }
    }

    pub fn as_libcamera(&self) -> Option<&LibcameraFrameMeta> {
        match self {
            Self::Libcamera(meta) => Some(meta),
            _ => None,
        }
    }

    pub fn as_native(&self) -> Option<&NativeFrameMeta> {
        match self {
            Self::Native(meta) => Some(meta),
            _ => None,
        }
    }

    pub fn as_uvc(&self) -> Option<&UvcFrameMeta> {
        match self {
            Self::Uvc(meta) => Some(meta),
            _ => None,
        }
    }

    /// The exposure the frame was taken with, in nanoseconds, when the backend reports it
    /// per frame (native and libcamera).
    pub fn exposure_ns(&self) -> Option<u64> {
        match self {
            Self::Native(meta) => Some(meta.exposure_ns),
            Self::Libcamera(meta) => meta.exposure_ns,
            Self::V4l2(_) | Self::Uvc(_) => None,
        }
    }

    /// Driver frame sequence number, when the backend reports one.
    pub fn sequence(&self) -> u32 {
        match self {
            Self::V4l2(meta) => meta.sequence,
            Self::Libcamera(meta) => meta.sequence,
            Self::Native(meta) => meta.sequence,
            Self::Uvc(meta) => meta.sequence,
        }
    }
}

/// Per-frame metadata of a UVC camera driven from userspace: the payload headers' device
/// clock values and when the frame crossed the bus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UvcFrameMeta {
    /// Frames assembled since the stream started (gaps: damaged frames dropped).
    pub sequence: u32,
    pub bytes_used: u32,
    /// Lost or failed packets, the ERR bit, a short or overflowing frame.
    pub error: bool,
    /// The frame ended on a FID toggle rather than EOF.
    pub no_eof: bool,
    /// The frame timestamp is the capture time from PTS (through SCR), not the arrival time.
    pub timestamp_from_pts: bool,
    /// Presentation time stamp: the device clock when the sensor captured the frame.
    pub pts: Option<u32>,
    /// The last source clock reference: device clock and the 11-bit USB frame number.
    pub scr_stc: Option<u32>,
    pub scr_sof: Option<u16>,
    /// The device clock's frequency (`dwClockFrequency`), Hz.
    pub clock_hz: u32,
    /// Arrival of the frame's first and last payloads (bus time, `CLOCK_MONOTONIC` ns).
    pub first_payload_ns: u64,
    pub last_payload_ns: u64,
}

impl UvcFrameMeta {
    /// The V4L2 buffer metadata `uvcvideo` would report for the frame (what recordings keep).
    pub fn as_v4l2(&self) -> V4l2FrameMeta {
        // V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC, plus V4L2_BUF_FLAG_ERROR for a damaged frame.
        let flags = 0x2000 | if self.error { 0x40 } else { 0 };
        V4l2FrameMeta {
            sequence: self.sequence,
            bytes_used: self.bytes_used,
            field: 1,
            flags,
            zero_copy: true,
        }
    }
}

/// Per-frame metadata of a sensor Styx drives itself: the buffer, and the exposure, gain and
/// frame timing that produced the frame (predicted from the control schedule, or read back from
/// the frame's embedded data when `verified`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeFrameMeta {
    /// Frame sequence number from the receiver (0 at stream start).
    pub sequence: u32,
    /// Payload bytes.
    pub bytes_used: u32,
    /// The receiver flagged the frame as corrupted.
    pub error: bool,
    /// Exposure time in nanoseconds.
    pub exposure_ns: u64,
    /// Analogue gain.
    pub analog_gain: f32,
    /// Digital gain (1 without one).
    pub digital_gain: f32,
    /// Frame duration in nanoseconds.
    pub frame_duration_ns: u64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// The values were read back from the frame rather than predicted.
    pub verified: bool,
}

impl NativeFrameMeta {
    /// Total gain.
    pub fn gain(&self) -> f32 {
        self.analog_gain * self.digital_gain
    }
}

impl PartialEq for NativeFrameMeta {
    fn eq(&self, o: &Self) -> bool {
        (
            self.sequence,
            self.bytes_used,
            self.error,
            self.exposure_ns,
            self.analog_gain.to_bits(),
            self.digital_gain.to_bits(),
            self.frame_duration_ns,
            self.frame_length,
            self.verified,
        ) == (
            o.sequence,
            o.bytes_used,
            o.error,
            o.exposure_ns,
            o.analog_gain.to_bits(),
            o.digital_gain.to_bits(),
            o.frame_duration_ns,
            o.frame_length,
            o.verified,
        )
    }
}

impl Eq for NativeFrameMeta {}

/// libcamera per-frame metadata: the buffer, and the exposure and gain the request's metadata
/// reports for the frame (`None` where libcamera did not report one, e.g. in a recording made
/// before these were kept).
#[derive(Debug, Clone, Copy, Default)]
pub struct LibcameraFrameMeta {
    /// Frame sequence number from the capture device; gaps indicate dropped frames.
    pub sequence: u32,
    /// Where the capture buffer memory came from, e.g. `dma-heap` or `libcamera-allocator`.
    pub buffer_memory: &'static str,
    /// `ExposureTime` as applied to the frame, in nanoseconds.
    pub exposure_ns: Option<u64>,
    /// `AnalogueGain` as applied to the frame.
    pub analogue_gain: Option<f32>,
    /// `DigitalGain` as applied to the frame (1 without one).
    pub digital_gain: Option<f32>,
    /// `FrameDuration` of the frame, in nanoseconds.
    pub frame_duration_ns: Option<u64>,
}

impl LibcameraFrameMeta {
    /// Total gain (analogue times digital), when libcamera reported the analogue gain.
    pub fn gain(&self) -> Option<f32> {
        self.analogue_gain
            .map(|analogue| analogue * self.digital_gain.unwrap_or(1.0))
    }
}

impl PartialEq for LibcameraFrameMeta {
    fn eq(&self, o: &Self) -> bool {
        let bits = |m: &Self| {
            (
                m.sequence,
                m.buffer_memory,
                m.exposure_ns,
                m.analogue_gain.map(f32::to_bits),
                m.digital_gain.map(f32::to_bits),
                m.frame_duration_ns,
            )
        };
        bits(self) == bits(o)
    }
}

impl Eq for LibcameraFrameMeta {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V4l2FrameMeta {
    pub sequence: u32,
    pub bytes_used: u32,
    pub field: u32,
    pub flags: u32,
    pub zero_copy: bool,
}

/// Metadata associated with a frame.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub format: MediaFormat,
    pub timestamp: u64,
    pub backend: Option<BackendFrameMeta>,
    /// When Styx received the frame (for [`FrameMeta::latency`]).
    pub capture_instant: Option<CaptureInstant>,
    pub residency: Option<FrameResidency>,
    pub mutability: FrameMutability,
    pub last_transition: Option<ResidencyTransition>,
    /// Where this frame's time went: capture latency and per-stage processing durations.
    pub timing: FrameTiming,
    /// When this frame is a region-of-interest view: the region in full-frame coordinates.
    pub crop: Option<crate::requirements::FrameRect>,
    /// Clock `timestamp` is expressed in, when the backend reports it.
    pub clock: Option<TimestampClock>,
    /// An inter-coded packet (H.264/H.265 P-frame): decoding it needs the packets before it, back
    /// to the last keyframe. `false` for raw frames, JPEG and keyframes.
    pub delta: bool,
    /// When the frame passed each step from the sensor to its consumer, and the copies made of
    /// it on the way (empty without the `path-metrics` feature; see [`super::hops`]).
    pub hops: FrameHops,
}

/// Per-frame latency breakdown. Backends fill `sensor_to_capture`; pipelines fill the stage
/// durations. Stages a frame did not pass through stay `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameTiming {
    /// Sensor timestamp (typically start of exposure or of readout) → frame handed to Styx.
    pub sensor_to_capture: Option<Duration>,
    pub decode: Option<Duration>,
    pub transform: Option<Duration>,
    pub hook: Option<Duration>,
    pub encode: Option<Duration>,
}

impl FrameTiming {
    /// Sum of the recorded processing stages.
    pub fn processing(&self) -> Duration {
        [self.decode, self.transform, self.hook, self.encode]
            .into_iter()
            .flatten()
            .sum()
    }

    /// Keep `self`'s values and fill gaps from `earlier` (e.g. timing from before a decoder
    /// replaced the frame's metadata).
    pub fn merged_with(self, earlier: FrameTiming) -> FrameTiming {
        FrameTiming {
            sensor_to_capture: self.sensor_to_capture.or(earlier.sensor_to_capture),
            decode: self.decode.or(earlier.decode),
            transform: self.transform.or(earlier.transform),
            hook: self.hook.or(earlier.hook),
            encode: self.encode.or(earlier.encode),
        }
    }
}

/// Latency summary for a frame at the moment [`FrameMeta::latency`] is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLatency {
    /// Sensor → capture backend delivery.
    pub sensor_to_capture: Option<Duration>,
    /// Time since the backend delivered the frame (queues, processing, consumer).
    pub since_capture: Option<Duration>,
    /// Recorded pipeline processing time (decode + transform + hook + encode).
    pub processing: Duration,
    /// Sensor → now, when both parts are known ("glass to app").
    pub total: Option<Duration>,
}

impl FrameMeta {
    pub fn new(format: MediaFormat, timestamp: u64) -> Self {
        Self {
            format,
            timestamp,
            backend: None,
            capture_instant: None,
            residency: None,
            mutability: FrameMutability::Mutable,
            last_transition: None,
            timing: FrameTiming::default(),
            crop: None,
            clock: None,
            delta: false,
            hops: FrameHops::new(),
        }
    }

    pub fn with_backend(mut self, backend: BackendFrameMeta) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn backend(&self) -> Option<&BackendFrameMeta> {
        self.backend.as_ref()
    }

    pub fn v4l2(&self) -> Option<&V4l2FrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_v4l2)
    }

    pub fn libcamera(&self) -> Option<&LibcameraFrameMeta> {
        self.backend
            .as_ref()
            .and_then(BackendFrameMeta::as_libcamera)
    }

    pub fn native(&self) -> Option<&NativeFrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_native)
    }

    pub fn uvc(&self) -> Option<&UvcFrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_uvc)
    }

    /// Driver frame sequence number, when the backend reports one (or, for a frame from
    /// another process, the one carried with its hops).
    pub fn sequence(&self) -> Option<u32> {
        self.backend
            .as_ref()
            .map(BackendFrameMeta::sequence)
            .or(self.hops.sequence())
    }

    /// Record when Styx received the frame (a [`CaptureInstant`], or with `std` an `Instant`).
    pub fn with_capture_instant(mut self, capture_instant: impl Into<CaptureInstant>) -> Self {
        self.capture_instant = Some(capture_instant.into());
        self
    }

    pub fn capture_instant(&self) -> Option<CaptureInstant> {
        self.capture_instant
    }

    /// Record how long ago the sensor captured this frame, using `clock` for `timestamp`.
    pub fn with_sensor_latency(self, clock: TimestampClock) -> Self {
        let timestamp = self.timestamp;
        self.with_sensor_latency_from(clock, timestamp)
    }

    /// Like [`FrameMeta::with_sensor_latency`] for backends whose sensor timestamp differs from
    /// the frame's buffer timestamp (e.g. libcamera `SensorTimestamp` vs. ISP completion time).
    pub fn with_sensor_latency_from(mut self, clock: TimestampClock, sensor_ns: u64) -> Self {
        self.timing.sensor_to_capture = clock.elapsed_since(sensor_ns);
        self
    }

    pub fn with_clock(mut self, clock: TimestampClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Record `native` as the timestamp's clock, converting the timestamp first when a
    /// conversion is given.
    pub fn in_clock(mut self, native: TimestampClock, conversion: Option<ClockConversion>) -> Self {
        match conversion {
            Some(conversion) => {
                self.timestamp = conversion.apply(self.timestamp);
                self.clock = Some(conversion.target());
            }
            None => self.clock = Some(native),
        }
        self
    }

    /// `timestamp` converted to `target`, when this frame's clock is known and both are system
    /// clocks. The offset is sampled now, so repeated calls can differ by a few nanoseconds.
    pub fn timestamp_in(&self, target: TimestampClock) -> Option<u64> {
        let conversion = ClockConversion::new(self.clock?, target)?;
        Some(conversion.apply(self.timestamp))
    }

    /// When the frame's exposure was half done, on [`FrameMeta::clock`]: the timestamp minus
    /// half the frame's applied exposure (docs/timestamps.md). The time to align a frame with
    /// other sensors (odometry, an IMU). For the native and libcamera backends, whose timestamp
    /// is the receiver's start of frame, right after the exposure ends; `None` for backends
    /// that do not report the exposure per frame, and for stream-relative timestamps. On a
    /// global shutter sensor (the OV9782) it is the whole frame's midpoint; on a rolling
    /// shutter one it is the first row's, each later row's being a line time later.
    pub fn exposure_midpoint(&self) -> Option<u64> {
        if matches!(self.clock?, TimestampClock::StreamRelative) {
            return None;
        }
        let exposure = self.backend.as_ref()?.exposure_ns()?;
        self.timestamp.checked_sub(exposure / 2)
    }

    /// [`FrameMeta::exposure_midpoint`] converted to `target` (as [`FrameMeta::timestamp_in`]).
    pub fn exposure_midpoint_in(&self, target: TimestampClock) -> Option<u64> {
        let conversion = ClockConversion::new(self.clock?, target)?;
        Some(conversion.apply(self.exposure_midpoint()?))
    }

    /// Latency summary as of now.
    #[cfg(feature = "std")]
    pub fn latency(&self) -> FrameLatency {
        self.latency_at(CaptureInstant::now())
    }

    /// Latency summary as of `now` (on the clock of [`FrameMeta::capture_instant`]).
    pub fn latency_at(&self, now: CaptureInstant) -> FrameLatency {
        let since_capture = self.capture_instant.map(|at| now.duration_since(at));
        FrameLatency {
            sensor_to_capture: self.timing.sensor_to_capture,
            since_capture,
            processing: self.timing.processing(),
            total: self
                .timing
                .sensor_to_capture
                .zip(since_capture)
                .map(|(a, b)| a + b),
        }
    }

    /// Carry capture context (backend metadata, capture instant, timing) from the frame a
    /// stage consumed into the metadata of the frame it produced, without overwriting values
    /// the stage set itself.
    pub fn inherit_capture_context(&mut self, input: &FrameMeta) {
        if self.backend.is_none() {
            self.backend = input.backend.clone();
        }
        if self.capture_instant.is_none() {
            self.capture_instant = input.capture_instant;
        }
        // Stages that keep the input timestamp (decoders, transforms) keep its clock too.
        if self.clock.is_none() && self.timestamp == input.timestamp {
            self.clock = input.clock;
        }
        self.timing = self.timing.merged_with(input.timing);
        self.hops.merge_from(&input.hops);
    }

    /// This frame's hops as exported, with the sequence number as the join key.
    pub fn hop_record(&self) -> HopRecord {
        HopRecord::new(self.sequence(), &self.hops)
    }

    /// The sensor timestamp on `CLOCK_MONOTONIC` ([`Hop::Sensor`]): `timestamp` when the frame's
    /// clock is monotonic, converted from boottime or realtime, `None` for stream-relative or
    /// unknown clocks.
    pub fn sensor_monotonic_ns(&self) -> Option<u64> {
        if self.timestamp == 0 {
            return None;
        }
        match self.clock? {
            TimestampClock::Monotonic => Some(self.timestamp),
            TimestampClock::StreamRelative => None,
            _ => self.timestamp_in(TimestampClock::Monotonic),
        }
    }

    pub fn with_residency(mut self, residency: FrameResidency) -> Self {
        self.residency = Some(residency);
        self
    }

    pub fn residency(&self) -> Option<FrameResidency> {
        self.residency
    }

    pub fn with_mutability(mut self, mutability: FrameMutability) -> Self {
        self.mutability = mutability;
        self
    }

    pub fn mutability(&self) -> FrameMutability {
        self.mutability
    }

    pub fn with_transition(mut self, transition: ResidencyTransition) -> Self {
        self.last_transition = Some(transition);
        self
    }

    pub fn last_transition(&self) -> Option<ResidencyTransition> {
        self.last_transition
    }
}

#[cfg(all(test, feature = "std"))]
mod timing_tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
    use std::time::Instant;

    fn meta() -> FrameMeta {
        let res = Resolution::new(2, 2).unwrap();
        FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), 0)
    }

    #[test]
    fn capture_instants_follow_instant() {
        let a = Instant::now();
        let b = a + Duration::from_millis(5);
        let (ca, cb) = (CaptureInstant::from(a), CaptureInstant::from(b));
        assert_eq!(cb.duration_since(ca), Duration::from_millis(5));
        assert_eq!(ca.duration_since(cb), Duration::ZERO);
        assert!(CaptureInstant::now() >= ca);
        #[cfg(target_os = "linux")]
        {
            let mono = TimestampClock::Monotonic.now_ns().unwrap();
            assert!(CaptureInstant::now().as_nanos().abs_diff(mono) < 50_000_000);
        }
        let meta = meta().with_capture_instant(CaptureInstant::from_nanos(1_000));
        let latency = meta.latency_at(CaptureInstant::from_nanos(4_000));
        assert_eq!(latency.since_capture, Some(Duration::from_nanos(3_000)));
    }

    #[test]
    fn inherit_keeps_stage_values_and_fills_gaps() {
        let mut input = meta().with_capture_instant(Instant::now());
        input.timing.sensor_to_capture = Some(Duration::from_millis(9));
        input.timing.decode = Some(Duration::from_millis(1));
        let mut output = meta();
        output.timing.decode = Some(Duration::from_millis(2));
        output.inherit_capture_context(&input);
        assert_eq!(
            output.timing.sensor_to_capture,
            Some(Duration::from_millis(9))
        );
        assert_eq!(output.timing.decode, Some(Duration::from_millis(2)));
        assert!(output.capture_instant.is_some());
        let latency = output.latency();
        assert_eq!(latency.processing, Duration::from_millis(2));
        assert!(latency.total.unwrap() >= Duration::from_millis(9));
    }

    #[test]
    fn stages_keep_the_clock_only_with_the_same_timestamp() {
        let input = meta().with_clock(TimestampClock::Boottime);
        let mut decoded = FrameMeta {
            timestamp: input.timestamp,
            ..meta()
        };
        decoded.inherit_capture_context(&input);
        assert_eq!(decoded.clock, Some(TimestampClock::Boottime));
        let mut retimed = FrameMeta {
            timestamp: input.timestamp + 1,
            ..meta()
        };
        retimed.inherit_capture_context(&input);
        assert_eq!(retimed.clock, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clock_conversion_round_trips_and_is_shared() {
        let now = TimestampClock::Monotonic.now_ns().unwrap();
        let meta = meta().with_clock(TimestampClock::Monotonic);
        let meta = FrameMeta {
            timestamp: now,
            ..meta
        };
        let in_boot = meta.timestamp_in(TimestampClock::Boottime).unwrap();
        let boot_now = TimestampClock::Boottime.now_ns().unwrap();
        assert!(boot_now.abs_diff(in_boot) < 50_000_000);
        let conversion =
            ClockConversion::new(TimestampClock::Boottime, TimestampClock::Realtime).unwrap();
        assert_eq!(conversion.apply(10) - conversion.apply(0), 10);
        assert!(
            ClockConversion::new(TimestampClock::StreamRelative, TimestampClock::Monotonic)
                .is_none()
        );
        assert!(meta.timestamp_in(TimestampClock::Monotonic) == Some(now));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn monotonic_elapsed_is_measured() {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        let ts = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
        let elapsed = TimestampClock::Monotonic.elapsed_since(ts).unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(TimestampClock::Monotonic.elapsed_since(u64::MAX).is_none());
    }
}

#[cfg(all(test, feature = "std"))]
mod exposure_midpoint_tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};

    fn meta(
        ts: u64,
        clock: Option<TimestampClock>,
        backend: Option<BackendFrameMeta>,
    ) -> FrameMeta {
        let res = Resolution::new(2, 2).unwrap();
        let mut m = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), ts);
        m.clock = clock;
        m.backend = backend;
        m
    }

    #[test]
    fn the_midpoint_is_half_the_applied_exposure_before_the_start_of_frame() {
        let native = BackendFrameMeta::Native(NativeFrameMeta {
            exposure_ns: 8_000_000,
            ..Default::default()
        });
        let m = meta(1_000_000_000, Some(TimestampClock::Monotonic), Some(native));
        assert_eq!(m.exposure_midpoint(), Some(996_000_000));
        assert_eq!(
            m.exposure_midpoint_in(TimestampClock::Monotonic),
            Some(996_000_000)
        );
        // Converted like the timestamp: the same offset between the two.
        let (t, mid) = (
            m.timestamp_in(TimestampClock::Boottime).unwrap(),
            m.exposure_midpoint_in(TimestampClock::Boottime).unwrap(),
        );
        assert!((t - mid).abs_diff(4_000_000) < 1_000, "{t} {mid}");

        let lc = BackendFrameMeta::Libcamera(LibcameraFrameMeta {
            exposure_ns: Some(3_931_000),
            ..Default::default()
        });
        let m = meta(10_000_000, Some(TimestampClock::Boottime), Some(lc));
        assert_eq!(m.exposure_midpoint(), Some(10_000_000 - 1_965_500));
    }

    #[test]
    fn no_midpoint_without_an_exposure_or_a_system_clock() {
        let native = || {
            Some(BackendFrameMeta::Native(NativeFrameMeta {
                exposure_ns: 1_000_000,
                ..Default::default()
            }))
        };
        assert_eq!(meta(5_000_000, None, native()).exposure_midpoint(), None);
        let relative = meta(5_000_000, Some(TimestampClock::StreamRelative), native());
        assert_eq!(relative.exposure_midpoint(), None);
        let unreported = BackendFrameMeta::Libcamera(LibcameraFrameMeta::default());
        let m = meta(5_000_000, Some(TimestampClock::Boottime), Some(unreported));
        assert_eq!(m.exposure_midpoint(), None);
        assert_eq!(
            meta(5_000_000, Some(TimestampClock::Monotonic), None).exposure_midpoint(),
            None
        );
    }
}

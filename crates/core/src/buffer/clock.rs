//! Clocks: when Styx received a frame ([`CaptureInstant`]), the clock a frame's timestamp is
//! on ([`TimestampClock`]) and conversions between them, and the platform's clocks for builds
//! without an OS ([`set_platform_clock`]).

use core::time::Duration;

/// When Styx received a frame: nanoseconds on a monotonic clock.
///
/// With `std` this is std's [`Instant`](std::time::Instant) timeline ([`CaptureInstant::now`],
/// `From<Instant>`): on Linux the nanoseconds are `CLOCK_MONOTONIC`'s (the clock of `Instant`
/// and of V4L2 buffer timestamps). Without `std` the platform passes its own monotonic
/// nanoseconds ([`CaptureInstant::from_nanos`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptureInstant(u64);

impl CaptureInstant {
    /// An instant `ns` nanoseconds into the monotonic clock.
    pub const fn from_nanos(ns: u64) -> Self {
        Self(ns)
    }

    /// The nanoseconds.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Time from `earlier` to `self`; zero when `earlier` is later.
    pub fn duration_since(self, earlier: CaptureInstant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    /// Time from `earlier` to `self`, `None` when `earlier` is later.
    pub fn checked_duration_since(self, earlier: CaptureInstant) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    /// Now.
    #[cfg(feature = "std")]
    pub fn now() -> Self {
        std::time::Instant::now().into()
    }

    /// Now, when a clock is known: with `std` always ([`CaptureInstant::now`]); without it
    /// the platform's monotonic clock ([`set_platform_clock`]).
    pub fn try_now() -> Option<Self> {
        #[cfg(feature = "std")]
        {
            Some(Self::now())
        }
        #[cfg(not(feature = "std"))]
        {
            platform_clock()?(TimestampClock::Monotonic).map(Self)
        }
    }

    /// Time since this instant (zero if it lies in the future), as `Instant::elapsed`.
    #[cfg(feature = "std")]
    pub fn elapsed(self) -> Duration {
        Self::now().duration_since(self)
    }
}

/// An `Instant` and the nanoseconds it maps to, sampled once.
#[cfg(feature = "std")]
fn instant_anchor() -> &'static (std::time::Instant, u64) {
    static ANCHOR: std::sync::OnceLock<(std::time::Instant, u64)> = std::sync::OnceLock::new();
    ANCHOR.get_or_init(|| {
        let at = std::time::Instant::now();
        // Linux: `Instant` is `CLOCK_MONOTONIC`, so name the same nanoseconds. Elsewhere an
        // arbitrary origin with room on both sides.
        let ns = TimestampClock::Monotonic
            .os_now_ns()
            .unwrap_or(u64::MAX / 2);
        (at, ns)
    })
}

#[cfg(feature = "std")]
impl From<std::time::Instant> for CaptureInstant {
    fn from(t: std::time::Instant) -> Self {
        let (at, ns) = *instant_anchor();
        let ns = match t.checked_duration_since(at) {
            Some(after) => ns.saturating_add(after.as_nanos().min(u128::from(u64::MAX)) as u64),
            None => {
                ns.saturating_sub(at.duration_since(t).as_nanos().min(u128::from(u64::MAX)) as u64)
            }
        };
        Self(ns)
    }
}
/// Clock a frame's `timestamp` is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TimestampClock {
    /// `CLOCK_MONOTONIC`: steady, stops during suspend (V4L2 buffers, `std::time::Instant`).
    Monotonic,
    /// `CLOCK_BOOTTIME`: steady, keeps counting during suspend (libcamera, Android sensors).
    Boottime,
    /// `CLOCK_REALTIME`: wall-clock nanoseconds since the Unix epoch; can jump.
    Realtime,
    /// Nanoseconds since the capture stream started (file, network and synthetic sources).
    StreamRelative,
}

/// A platform's clocks, for builds and targets without an OS clock: the time on `clock` in
/// nanoseconds, or `None` when the platform does not keep it. A firmware passes its timer
/// (`set_platform_clock(|c| matches!(c, TimestampClock::Monotonic).then(ticks_ns))`), as it
/// gives `styx-runtime` its `Clock`.
pub type PlatformClock = fn(TimestampClock) -> Option<u64>;

static PLATFORM_CLOCK: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Sets the clocks [`TimestampClock::now_ns`] (and so [`ClockSource::stamp_now`] and
/// [`CaptureInstant::try_now`] without `std`) read. When set, it is used with `std` too (tests,
/// simulated time); otherwise `std` reads the OS clocks and a `no_std` build has none.
pub fn set_platform_clock(clock: PlatformClock) {
    PLATFORM_CLOCK.store(clock as *mut (), core::sync::atomic::Ordering::Release);
}

fn platform_clock() -> Option<PlatformClock> {
    let ptr = PLATFORM_CLOCK.load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: only `set_platform_clock` stores, and it stores a `PlatformClock`.
    (!ptr.is_null()).then(|| unsafe { core::mem::transmute::<*mut (), PlatformClock>(ptr) })
}

impl TimestampClock {
    /// Current time on this clock in nanoseconds; `None` for stream-relative time or when the
    /// clock is unavailable on this platform. The platform's clock when one is set
    /// ([`set_platform_clock`]), else with `std` the OS's.
    pub fn now_ns(self) -> Option<u64> {
        if let Some(clock) = platform_clock() {
            return clock(self);
        }
        self.os_now_ns()
    }

    fn os_now_ns(self) -> Option<u64> {
        #[cfg(all(feature = "std", target_os = "linux"))]
        {
            let clock = match self {
                Self::Monotonic => libc::CLOCK_MONOTONIC,
                Self::Boottime => libc::CLOCK_BOOTTIME,
                Self::Realtime => libc::CLOCK_REALTIME,
                Self::StreamRelative => return None,
            };
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is valid writable storage for clock_gettime.
            if unsafe { libc::clock_gettime(clock, &mut now) } != 0 {
                return None;
            }
            (now.tv_sec as u64)
                .checked_mul(1_000_000_000)?
                .checked_add(now.tv_nsec as u64)
        }
        #[cfg(all(feature = "std", not(target_os = "linux")))]
        {
            match self {
                Self::Realtime => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_nanos() as u64),
                _ => None,
            }
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = self;
            None
        }
    }

    /// Time elapsed since `timestamp_ns` on this clock, or `None` if the clock is unavailable
    /// or the timestamp lies in the future.
    pub fn elapsed_since(self, timestamp_ns: u64) -> Option<Duration> {
        self.now_ns()?
            .checked_sub(timestamp_ns)
            .map(Duration::from_nanos)
    }
}

/// Which clock capture backends should stamp frames with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClockSource {
    /// Keep each backend's own clock (libcamera: boottime, V4L2: usually monotonic, files and
    /// network streams: stream-relative). `FrameMeta::clock` says which one.
    #[default]
    Native,
    Monotonic,
    Boottime,
    Realtime,
}

impl ClockSource {
    pub fn clock(self) -> Option<TimestampClock> {
        match self {
            Self::Native => None,
            Self::Monotonic => Some(TimestampClock::Monotonic),
            Self::Boottime => Some(TimestampClock::Boottime),
            Self::Realtime => Some(TimestampClock::Realtime),
        }
    }
    /// Conversion from a backend's `native` clock to this source; `None` for `Native` or when
    /// the clocks cannot be related (stream-relative time).
    pub fn conversion_from(self, native: TimestampClock) -> Option<ClockConversion> {
        ClockConversion::new(native, self.clock()?)
    }

    /// Timestamp for a frame arriving now on a source without its own clock: the configured
    /// clock's current time, or `stream_elapsed` as stream-relative time for `Native`.
    pub fn stamp_now(self, stream_elapsed: Duration) -> (u64, TimestampClock) {
        self.clock()
            .and_then(|clock| Some((clock.now_ns()?, clock)))
            .unwrap_or((
                stream_elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
                TimestampClock::StreamRelative,
            ))
    }
}

/// A fixed offset between two system clocks, sampled once so related timestamps (e.g. a frame
/// and its pyramid companion) convert identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockConversion {
    target: TimestampClock,
    offset_ns: i128,
}

impl ClockConversion {
    /// Conversion from `from` to `to`; `None` if either is stream-relative or unavailable.
    pub fn new(from: TimestampClock, to: TimestampClock) -> Option<Self> {
        let offset_ns = if from == to {
            0
        } else {
            i128::from(to.now_ns()?) - i128::from(from.now_ns()?)
        };
        Some(Self {
            target: to,
            offset_ns,
        })
    }

    pub fn target(&self) -> TimestampClock {
        self.target
    }

    pub fn apply(&self, timestamp_ns: u64) -> u64 {
        (i128::from(timestamp_ns) + self.offset_ns).clamp(0, i128::from(u64::MAX)) as u64
    }
}

#[cfg(all(test, not(feature = "std")))]
mod tests {
    use super::*;

    fn board(clock: TimestampClock) -> Option<u64> {
        matches!(clock, TimestampClock::Monotonic).then_some(42_000)
    }

    #[test]
    fn the_platform_clock_is_the_clock_without_std() {
        assert_eq!(CaptureInstant::try_now(), None);
        set_platform_clock(board);
        assert_eq!(TimestampClock::Monotonic.now_ns(), Some(42_000));
        assert_eq!(TimestampClock::Realtime.now_ns(), None);
        assert_eq!(
            CaptureInstant::try_now(),
            Some(CaptureInstant::from_nanos(42_000))
        );
        assert_eq!(
            ClockSource::Monotonic.stamp_now(Duration::ZERO),
            (42_000, TimestampClock::Monotonic)
        );
    }
}

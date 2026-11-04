//! When a frame's first rows are in memory, from what the stock driver gives: its `FRAME_SYNC`
//! event, a delay from the sensor's line time, and a marker. The row after the slice line starts
//! with a marker in every raw buffer before it is queued; a marker gone at the delay's end means
//! the rows above it have landed, whatever the timing did. One still there is an early fire: the
//! frame runs whole. No thread: the caller's wait wakes for [`LineTimer::due`].

use std::cell::Cell;
use std::time::Duration;

use styx_kernel::Mapping;

use super::LineEvent;

/// Written where the row after the slice line starts. Sensor data equal to it would read as
/// not landed, and the frame would run whole.
const MARKER: [u8; 16] = [
    0x5a, 0xc3, 0x96, 0x3d, 0xe1, 0x7b, 0x18, 0xa4, 0x2f, 0xd0, 0x6e, 0x85, 0xb9, 0x43, 0xf7, 0x0c,
];

/// Upper bounds in µs of [`LineTimerCounts::late`]'s first buckets; the last takes the rest.
pub const LATE_BOUNDS_US: [u64; 5] = [50, 100, 250, 500, 1000];

/// What the timer did: wake-ups that came late are not early fires, and only early fires cost a
/// frame its slice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineTimerCounts {
    /// `FRAME_SYNC` events read.
    pub starts: u64,
    /// Rows found in memory: line events raised.
    pub fired: u64,
    /// Marker still there at the delay's end: the frame runs whole.
    pub early: u64,
    /// The frame in flight was not the event's (the caller is a frame behind, or the stream
    /// just started): the frame runs whole.
    pub stale: u64,
    /// A frame start arrived with the last one's delay still running.
    pub replaced: u64,
    /// Time from the delay's end to the check, by [`LATE_BOUNDS_US`].
    pub late: [u64; LATE_BOUNDS_US.len() + 1],
    /// The longest of those.
    pub worst_late: Duration,
}

#[derive(Clone, Copy)]
struct Due {
    sequence: u32,
    frame_start: Duration,
    at: Duration,
}

/// Where a buffer's marker goes.
struct Row(*mut [u8; 16]);

// SAFETY: the pointer is into a mapping the timer owns (or a test's memory), and it is only
// read and written through the timer.
unsafe impl Send for Row {}

impl Row {
    fn arm(&self) {
        // SAFETY: 16 bytes of a raw buffer that is not queued, as `LineTimer::new` requires.
        unsafe { self.0.write_volatile(MARKER) };
    }

    fn landed(&self) -> bool {
        // SAFETY: 16 mapped bytes, as `LineTimer::new` requires. The hardware may write them
        // at any time, so this is a volatile read, of memory mapped uncached.
        unsafe { self.0.read_volatile() != MARKER }
    }
}

pub(super) struct LineTimer {
    line: u32,
    delay: Duration,
    rows: Vec<Row>,
    _maps: Vec<Vec<Mapping>>,
    due: Cell<Option<Due>>,
    /// The sequence of the last frame taken from the front end.
    taken: Cell<Option<u32>>,
    counts: Cell<LineTimerCounts>,
}

impl LineTimer {
    /// A timer for `line` rows over raw buffers mapped writable, `offset` the start of the row
    /// after them. `None` when a buffer is too short for that.
    pub(super) fn new(line: u32, mut maps: Vec<Vec<Mapping>>, offset: usize) -> Option<Self> {
        let mut rows = Vec::with_capacity(maps.len());
        for m in &mut maps {
            let bytes = m.first_mut()?.as_mut_slice().get_mut(offset..offset + 16)?;
            rows.push(Row(bytes.as_mut_ptr().cast()));
        }
        Some(Self::with_rows(line, rows, maps))
    }

    fn with_rows(line: u32, rows: Vec<Row>, maps: Vec<Vec<Mapping>>) -> Self {
        Self {
            line,
            delay: Duration::ZERO,
            rows,
            _maps: maps,
            due: Cell::new(None),
            taken: Cell::new(None),
            counts: Cell::default(),
        }
    }

    /// The time from a frame's start to the check.
    pub(super) fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Buffer `index` is about to be queued.
    pub(super) fn arm(&self, index: u32) {
        if let Some(r) = self.rows.get(index as usize) {
            r.arm();
        }
    }

    /// A `FRAME_SYNC` event: frame `sequence` started at `frame_start`.
    pub(super) fn frame_start(&self, sequence: u32, frame_start: Duration) {
        let mut c = self.counts.get();
        c.starts += 1;
        c.replaced += u64::from(self.due.get().is_some());
        self.counts.set(c);
        self.due.set(Some(Due {
            sequence,
            frame_start,
            at: frame_start + self.delay,
        }));
    }

    /// Frame `sequence` left the front end.
    pub(super) fn frame_taken(&self, sequence: u32) {
        self.taken.set(Some(sequence));
    }

    /// When the next check is due (`CLOCK_MONOTONIC`).
    pub(super) fn due(&self) -> Option<Duration> {
        self.due.get().map(|d| d.at)
    }

    /// The check, once due: the line event when buffer `in_flight` is the frame's and has its
    /// rows. `None` otherwise, and the frame runs whole.
    pub(super) fn check(&self, now: Duration, in_flight: Option<u32>) -> Option<LineEvent> {
        let due = self.due.get().filter(|d| now >= d.at)?;
        self.due.set(None);
        let mut c = self.counts.get();
        let late = now - due.at;
        let us = late.as_micros() as u64;
        let bucket = LATE_BOUNDS_US.iter().position(|b| us < *b);
        c.late[bucket.unwrap_or(LATE_BOUNDS_US.len())] += 1;
        c.worst_late = c.worst_late.max(late);
        let next = self.taken.get().map(|t| t.wrapping_add(1));
        let row = in_flight
            .filter(|_| next == Some(due.sequence))
            .and_then(|i| self.rows.get(i as usize));
        let event = match row {
            None => {
                c.stale += 1;
                None
            }
            Some(r) if !r.landed() => {
                c.early += 1;
                None
            }
            Some(_) => {
                c.fired += 1;
                Some(LineEvent {
                    sequence: due.sequence,
                    line: self.line,
                    at: now,
                    frame_start: due.frame_start,
                })
            }
        };
        self.counts.set(c);
        event
    }

    pub(super) fn counts(&self) -> LineTimerCounts {
        self.counts.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn timer(buffers: &mut [[u8; 16]]) -> LineTimer {
        let rows = buffers.iter_mut().map(|b| Row(b as *mut _)).collect();
        let mut t = LineTimer::with_rows(600, rows, Vec::new());
        t.set_delay(5 * MS);
        t.arm(0);
        t.arm(1);
        t.frame_taken(6);
        t
    }

    #[test]
    fn rows_in_memory_raise_the_event_at_the_delays_end() {
        let mut b = [[0; 16]; 2];
        let t = timer(&mut b);
        t.frame_start(7, 100 * MS);
        assert_eq!(t.due(), Some(105 * MS));
        // SAFETY: the timer is not running; this is what the hardware's write does.
        unsafe { (&raw mut b[1]).write_volatile([1; 16]) };
        assert_eq!(t.check(104 * MS, Some(1)), None, "not due");
        let ev = t.check(105 * MS, Some(1)).unwrap();
        assert_eq!((ev.sequence, ev.line, ev.frame_start), (7, 600, 100 * MS));
        assert_eq!(t.due(), None);
        assert_eq!(t.counts().fired, 1);
    }

    #[test]
    fn a_marker_still_there_runs_the_frame_whole() {
        let mut b = [[0; 16]; 2];
        let t = timer(&mut b);
        t.frame_start(7, 100 * MS);
        assert_eq!(t.check(105 * MS, Some(1)), None);
        let c = t.counts();
        assert_eq!((c.fired, c.early, c.stale), (0, 1, 0));
    }

    #[test]
    fn a_late_wake_up_is_not_an_early_fire() {
        let mut b = [[0; 16]; 2];
        let t = timer(&mut b);
        t.frame_start(7, 100 * MS);
        // SAFETY: as above.
        unsafe { (&raw mut b[0]).write_volatile([1; 16]) };
        assert!(t.check(107 * MS, Some(0)).is_some());
        let c = t.counts();
        assert_eq!((c.early, c.late[LATE_BOUNDS_US.len()]), (0, 1));
        assert_eq!(c.worst_late, 2 * MS);
    }

    #[test]
    fn a_frame_not_in_flight_runs_whole() {
        let mut b = [[0; 16]; 2];
        let t = timer(&mut b);
        // The caller still holds frame 7 in the queue: the buffer in front is not frame 8's,
        // and its marker is long gone.
        t.frame_start(8, 100 * MS);
        // SAFETY: as above.
        unsafe { (&raw mut b[0]).write_volatile([1; 16]) };
        assert_eq!(t.check(105 * MS, Some(0)), None);
        assert_eq!(t.counts().stale, 1);
    }

    #[test]
    fn a_second_start_replaces_the_first() {
        let mut b = [[0; 16]; 2];
        let t = timer(&mut b);
        t.frame_start(7, 100 * MS);
        t.frame_start(8, 108 * MS);
        assert_eq!(t.due(), Some(113 * MS));
        assert_eq!(t.counts().replaced, 1);
    }
}

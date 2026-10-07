//! The per-frame sidecar `<name>.frames.csv`: one row per recorded frame (row `i` is frame `i`
//! of the raw file, or still `i`), with the sensor timestamp, its clock, the sequence number and
//! the frames missing before it.

use std::fmt::Write as _;

use styx::prelude::*;

pub const CSV_HEADER: &str = "frame,sequence,timestamp_ns,clock,dropped_before,gap_source,\
received_monotonic_ns,exposure_us,analogue_gain,frame_duration_us,file";

/// How `dropped_before` was worked out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapSource {
    /// The first frame: nothing before it.
    First,
    /// From the sequence numbers (exact).
    Sequence,
    /// From the timestamps and the frame interval (no sequence numbers).
    Timestamp,
    /// The sequence went backwards (the capture restarted): counting starts again.
    Reset,
    /// Neither sequence numbers nor a frame interval: unknown (0).
    Unknown,
    /// A still: the frames between stills are not recorded on purpose.
    Still,
}

impl GapSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Sequence => "sequence",
            Self::Timestamp => "timestamp",
            Self::Reset => "reset",
            Self::Unknown => "unknown",
            Self::Still => "still",
        }
    }
}

pub fn clock_name(clock: Option<TimestampClock>) -> &'static str {
    match clock {
        Some(TimestampClock::Monotonic) => "monotonic",
        Some(TimestampClock::Boottime) => "boottime",
        Some(TimestampClock::Realtime) => "realtime",
        Some(TimestampClock::StreamRelative) => "stream",
        None => "",
    }
}

/// One sidecar row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// Index in the recording (raw file frame, or still number).
    pub index: u64,
    pub sequence: Option<u32>,
    /// The frame's sensor timestamp, ns on `clock`.
    pub timestamp_ns: u64,
    pub clock: Option<TimestampClock>,
    /// Camera frames missing between the previous row and this one.
    pub dropped_before: u64,
    pub gap_source: Option<GapSource>,
    /// When the recorder received it (`CLOCK_MONOTONIC` ns).
    pub received_ns: Option<u64>,
    /// Per-frame exposure, gain and frame duration where the frame carries them (a sensor Styx
    /// drives, opened directly).
    pub exposure_us: Option<f64>,
    pub analogue_gain: Option<f32>,
    pub frame_duration_us: Option<f64>,
    /// The still's file (stills only).
    pub file: Option<String>,
}

impl Row {
    /// The row for `frame` (gap fields left for [`GapTracker`]).
    pub fn of(frame: &FrameLease, index: u64) -> Self {
        let meta = frame.meta();
        let native = meta.backend.as_ref().and_then(|b| b.as_native());
        Self {
            index,
            sequence: meta.sequence(),
            timestamp_ns: meta.timestamp,
            clock: meta.clock,
            received_ns: TimestampClock::Monotonic.now_ns(),
            exposure_us: native.map(|n| n.exposure_ns as f64 / 1000.0),
            analogue_gain: native.map(|n| n.analog_gain),
            frame_duration_us: native.map(|n| n.frame_duration_ns as f64 / 1000.0),
            ..Self::default()
        }
    }

    /// The CSV line (with its newline) appended to `out`.
    pub fn write_csv(&self, out: &mut String) {
        let opt = |out: &mut String, v: Option<String>| {
            if let Some(v) = v {
                out.push_str(&v);
            }
        };
        let _ = write!(out, "{},", self.index);
        opt(out, self.sequence.map(|s| s.to_string()));
        let _ = write!(
            out,
            ",{},{},{},{},",
            self.timestamp_ns,
            clock_name(self.clock),
            self.dropped_before,
            self.gap_source.map_or("", GapSource::name)
        );
        opt(out, self.received_ns.map(|v| v.to_string()));
        out.push(',');
        opt(out, self.exposure_us.map(|v| format!("{v}")));
        out.push(',');
        opt(out, self.analogue_gain.map(|v| format!("{v}")));
        out.push(',');
        opt(out, self.frame_duration_us.map(|v| format!("{v}")));
        out.push(',');
        opt(out, self.file.clone());
        out.push('\n');
    }
}

/// Works out the frames missing between consecutive recorded frames.
#[derive(Debug, Default)]
pub struct GapTracker {
    last_sequence: Option<u32>,
    last_timestamp: Option<u64>,
    /// The nominal frame interval, ns, for timestamps without sequence numbers.
    pub interval_ns: Option<u64>,
    /// Missing frames so far.
    pub total: u64,
    /// Gaps (places where at least one frame is missing).
    pub gaps: u64,
}

impl GapTracker {
    pub fn new(interval_ns: Option<u64>) -> Self {
        Self {
            interval_ns,
            ..Self::default()
        }
    }

    /// Fill `row`'s `dropped_before` and `gap_source`, and count it as recorded.
    pub fn observe(&mut self, row: &mut Row) {
        self.fill(row);
        self.commit(row);
    }

    /// Fill `row`'s `dropped_before` and `gap_source` from the last recorded frame.
    pub fn fill(&self, row: &mut Row) {
        let (dropped, source) = match (self.last_sequence, row.sequence) {
            (Some(last), Some(seq)) => {
                let step = seq.wrapping_sub(last);
                if step == 0 || step > u32::MAX / 2 {
                    (0, GapSource::Reset)
                } else {
                    (u64::from(step - 1), GapSource::Sequence)
                }
            }
            _ => match (self.last_timestamp, self.interval_ns) {
                (None, _) => (0, GapSource::First),
                (Some(last), Some(interval)) if interval > 0 => {
                    let dt = row.timestamp_ns.saturating_sub(last);
                    let frames = (dt + interval / 2) / interval;
                    (frames.saturating_sub(1), GapSource::Timestamp)
                }
                (Some(_), _) => (0, GapSource::Unknown),
            },
        };
        row.dropped_before = dropped;
        row.gap_source = Some(source);
    }

    /// `row` (filled) was recorded: later gaps count from it. A frame that was not recorded
    /// (the disk writer had no room) is not committed, so the next row counts it as dropped.
    pub fn commit(&mut self, row: &Row) {
        self.last_sequence = row.sequence;
        self.last_timestamp = Some(row.timestamp_ns);
        self.total += row.dropped_before;
        self.gaps += u64::from(row.dropped_before > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sequence: Option<u32>, timestamp_ns: u64) -> Row {
        Row {
            sequence,
            timestamp_ns,
            ..Row::default()
        }
    }

    #[test]
    fn counts_sequence_gaps() {
        let mut t = GapTracker::new(None);
        let mut gaps = Vec::new();
        for seq in [10, 11, 14, 15, 17] {
            let mut r = row(Some(seq), 0);
            t.observe(&mut r);
            gaps.push((r.dropped_before, r.gap_source.unwrap()));
        }
        assert_eq!(
            gaps,
            [
                (0, GapSource::First),
                (0, GapSource::Sequence),
                (2, GapSource::Sequence),
                (0, GapSource::Sequence),
                (1, GapSource::Sequence)
            ]
        );
        assert_eq!((t.total, t.gaps), (3, 2));
        // Backwards: a restart, not 4 billion drops.
        let mut r = row(Some(0), 0);
        t.observe(&mut r);
        assert_eq!(
            (r.dropped_before, r.gap_source),
            (0, Some(GapSource::Reset))
        );
    }

    #[test]
    fn falls_back_to_timestamps() {
        let mut t = GapTracker::new(Some(33_333_333));
        let mut out = Vec::new();
        for ts in [0u64, 33_333_333, 133_333_333, 166_000_000] {
            let mut r = row(None, ts);
            t.observe(&mut r);
            out.push(r.dropped_before);
        }
        assert_eq!(out, [0, 0, 2, 0]);
        let mut t = GapTracker::new(None);
        for ts in [0u64, 99] {
            let mut r = row(None, ts);
            t.observe(&mut r);
            assert_eq!(r.dropped_before, 0);
        }
    }

    #[test]
    fn csv_rows_match_the_header() {
        let mut s = String::new();
        Row {
            index: 3,
            sequence: Some(7),
            timestamp_ns: 123,
            clock: Some(TimestampClock::Boottime),
            dropped_before: 1,
            gap_source: Some(GapSource::Sequence),
            received_ns: Some(456),
            exposure_us: Some(10_000.0),
            analogue_gain: Some(2.5),
            frame_duration_us: None,
            file: None,
        }
        .write_csv(&mut s);
        assert_eq!(s, "3,7,123,boottime,1,sequence,456,10000,2.5,,\n");
        assert_eq!(
            s.trim_end().split(',').count(),
            CSV_HEADER.split(',').count()
        );
    }
}

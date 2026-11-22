//! What the reader measures from each frame: identity (gaps, duplicates, order), capture times
//! (the rate), capture-to-reader latency, and the ISP's time. Each record is a few integer
//! operations and a histogram bump; nothing allocates per frame.

use crate::hist::Histogram;
use styx_controls::MadeBy;

/// Counts and times over the frames one reader took.
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// Frames the reader handed over.
    pub delivered: u64,
    /// Frames whose id was not the one after the previous id (a skip in the sensor's sequence).
    pub gaps: u64,
    /// The largest run of ids missing at one gap.
    pub largest_gap: u64,
    /// The time gap across each producer stall, in ns: from the time the reader had the last frame
    /// before it to the time it had the first after it (the time the reader waited). Not gaps: no frame is counted lost from a stall (the sensor made none while it
    /// was stalled, and a rebind restarts the sequence).
    pub stall_gaps_ns: Vec<u64>,
    /// The next frame follows a stall: its id is not compared with the last one, and its time gap is
    /// recorded in `stall_gaps_ns`.
    stall_pending: bool,
    /// Frames whose id was the same as the previous one.
    pub duplicates: u64,
    /// Frames whose id was lower than the previous one.
    pub out_of_order: u64,
    /// Frames whose metadata blob did not decode (a version this build does not read, or damage).
    pub metadata_refused: u64,
    /// The id and capture time of the first and last frame.
    pub first_id: Option<u64>,
    pub last_id: Option<u64>,
    pub first_capture_ns: Option<u64>,
    pub last_capture_ns: Option<u64>,
    prev_capture_ns: Option<u64>,
    /// When the reader had the last frame (the time now at its record).
    prev_seen_ns: Option<u64>,
    /// Gaps between consecutive capture times (ns).
    pub intervals: Histogram,
    /// Capture to reader, now minus the capture time (ns). Negative values are counted apart.
    pub latency: Histogram,
    pub negative_latency: u64,
    /// The longest time from the previous frame's capture to the reader having the next frame
    /// (ns); the first frame after a producer stall is left out. This is what says whether a reader kept up: the frame it
    /// wanted next was this old, less one frame period, when it came back for it. A frame the
    /// ring skipped is not in `latency` (the reader got a younger one instead); it is here.
    pub since_previous_capture_max: Option<u64>,
    /// The first frames one by one, `[id, capture time, time the reader had it]` in ns, up to
    /// the room reserved by [`FrameStats::keep_trace`]. Empty unless asked for.
    pub trace: Vec<[u64; 3]>,
    /// Frames and capture-to-reader latency by what made each frame (the metadata's made-by
    /// byte): the camera itself (a raw socket), or an ISP on a processed socket, back end or soft.
    /// The latency is the same clock as `latency`: the sensor's capture time to the reader.
    pub made_by: [MadeByStats; 3],
    /// Frames whose metadata named no maker this build reads.
    pub made_by_unknown: u64,
}

/// One maker's frames and their capture-to-reader latency (see [`FrameStats::made_by`]).
#[derive(Debug, Clone, Default)]
pub struct MadeByStats {
    pub delivered: u64,
    pub latency: Histogram,
}

/// The makers in [`FrameStats::made_by`]'s order.
pub const MAKERS: [MadeBy; 3] = [MadeBy::Camera, MadeBy::Pisp, MadeBy::Soft];

const fn maker_index(made_by: MadeBy) -> usize {
    match made_by {
        MadeBy::Camera => 0,
        MadeBy::Pisp => 1,
        MadeBy::Soft => 2,
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStats {
    pub fn new() -> Self {
        Self {
            delivered: 0,
            gaps: 0,
            largest_gap: 0,
            duplicates: 0,
            out_of_order: 0,
            metadata_refused: 0,
            first_id: None,
            stall_gaps_ns: Vec::new(),
            stall_pending: false,
            last_id: None,
            first_capture_ns: None,
            last_capture_ns: None,
            prev_capture_ns: None,
            prev_seen_ns: None,
            intervals: Histogram::new(),
            latency: Histogram::new(),
            negative_latency: 0,
            since_previous_capture_max: None,
            trace: Vec::new(),
            made_by: Default::default(),
            made_by_unknown: 0,
        }
    }

    /// Keeps the first `frames` frames one by one. The room is taken here, once, so recording
    /// a frame allocates nothing.
    pub fn keep_trace(&mut self, frames: usize) {
        self.trace = Vec::with_capacity(frames);
    }

    /// Records one frame: its id, its capture time, the time now (when the reader has it), and
    /// whether its metadata decoded.
    pub fn record(&mut self, id: u64, capture_ns: u64, now_ns: u64, metadata_ok: bool) {
        self.delivered += 1;
        if self.trace.len() < self.trace.capacity() {
            self.trace.push([id, capture_ns, now_ns]);
        }
        if !metadata_ok {
            self.metadata_refused += 1;
        }
        match self.last_id {
            None => {
                self.first_id.get_or_insert(id);
            }
            Some(prev) if id == prev => self.duplicates += 1,
            Some(prev) if id < prev => self.out_of_order += 1,
            Some(prev) if id > prev + 1 => {
                self.gaps += 1;
                self.largest_gap = self.largest_gap.max(id - prev - 1);
            }
            Some(_) => {}
        }
        if self.last_id.is_none_or(|prev| id > prev) {
            self.last_id = Some(id);
        }
        if self.first_capture_ns.is_none() {
            self.first_capture_ns = Some(capture_ns);
        }
        if self.stall_pending {
            if let Some(prev) = self.prev_seen_ns {
                self.stall_gaps_ns.push(now_ns.saturating_sub(prev));
            }
            self.stall_pending = false;
        } else if let Some(prev) = self.prev_capture_ns {
            self.intervals.record(capture_ns.saturating_sub(prev));
            let since = now_ns.saturating_sub(prev);
            self.since_previous_capture_max = Some(
                self.since_previous_capture_max
                    .map_or(since, |m| m.max(since)),
            );
        }
        self.prev_seen_ns = Some(now_ns);
        self.prev_capture_ns = Some(capture_ns);
        self.last_capture_ns = Some(capture_ns);
        match now_ns.checked_sub(capture_ns) {
            Some(latency) => self.latency.record(latency),
            None => self.negative_latency += 1,
        }
    }

    /// Records what made the frame just recorded, with its capture-to-reader latency. `None`
    /// when the metadata named no maker this build reads.
    pub fn record_made_by(&mut self, made_by: Option<MadeBy>, capture_ns: u64, now_ns: u64) {
        let Some(made_by) = made_by else {
            self.made_by_unknown += 1;
            return;
        };
        let stats = &mut self.made_by[maker_index(made_by)];
        stats.delivered += 1;
        if let Some(latency) = now_ns.checked_sub(capture_ns) {
            stats.latency.record(latency);
        }
    }

    /// The camera stalled and came back: the next frame's id is not compared with the last one (a
    /// sequence that goes on and one that restarts are both fine here), and its time gap is the
    /// stall's.
    pub fn across_stall(&mut self) {
        self.last_id = None;
        self.stall_pending = true;
    }

    /// The mean capture interval over the run, in ns: the capture time from the first frame to the
    /// last, over the sensor's frame ids in between (one id per frame). A frame the ring skipped
    /// adds its time to the span and one to the ids, so the mean stays the sensor's frame time.
    /// `None` with fewer than two frames, or when the ids do not advance.
    pub fn mean_interval_ns(&self) -> Option<f64> {
        let (first, last) = (self.first_capture_ns?, self.last_capture_ns?);
        let (first_id, last_id) = (self.first_id?, self.last_id?);
        if last <= first || last_id <= first_id {
            return None;
        }
        Some((last - first) as f64 / (last_id - first_id) as f64)
    }

    /// The measured frame rate from the mean interval, frames per second.
    pub fn measured_fps(&self) -> Option<f64> {
        self.mean_interval_ns().map(|ns| 1e9 / ns)
    }
}

/// The ISP's time per frame and its output pool's counts.
#[derive(Debug, Clone)]
pub struct IspStats {
    pub times: Histogram,
    /// Frames the ISP refused (an error from the stage), not counted in `times`.
    pub errors: u64,
    /// Acquires of the output pool that found every buffer held.
    pub pool_misses: u64,
    pub pool_acquired: u64,
    pub pool_buffers: usize,
    pub output: String,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::{FrameStats, MadeBy};

    #[test]
    fn a_reader_held_up_past_skipped_frames_shows_it_since_the_previous_capture() {
        // Frames every 10 ns. The reader has 1 at 15, then is held up and the ring skips 2..4:
        // it has 5 at 52, only 2 ns after its capture. Its latency says nothing of the hold-up;
        // the time since the previous capture (10) does: 42, so frame 2 was 32 old when asked for.
        let mut s = FrameStats::new();
        s.record(1, 10, 15, true);
        s.record(5, 50, 52, true);
        assert_eq!(s.latency.max(), Some(5));
        assert_eq!(s.since_previous_capture_max, Some(42));
        s.record(6, 60, 61, true);
        assert_eq!(s.since_previous_capture_max, Some(42));
        // Across a producer stall the wait is the stall's, not the reader's.
        s.across_stall();
        s.record(0, 1000, 1001, true);
        assert_eq!(s.since_previous_capture_max, Some(42));
    }

    #[test]
    fn frames_are_counted_by_maker_with_their_latency() {
        let mut s = FrameStats::new();
        s.record_made_by(Some(MadeBy::Pisp), 100, 400);
        s.record_made_by(Some(MadeBy::Pisp), 200, 400);
        s.record_made_by(Some(MadeBy::Soft), 300, 1300);
        s.record_made_by(None, 0, 1);
        let [camera, pisp, soft] = &s.made_by;
        assert_eq!(
            (camera.delivered, pisp.delivered, soft.delivered),
            (0, 2, 1)
        );
        assert_eq!(
            (pisp.latency.min(), pisp.latency.max()),
            (Some(200), Some(300))
        );
        assert_eq!(soft.latency.min(), Some(1000));
        assert_eq!(s.made_by_unknown, 1);
    }

    #[test]
    fn the_trace_keeps_the_first_frames_and_never_grows() {
        let mut s = FrameStats::new();
        s.record(1, 10, 15, true);
        assert!(s.trace.is_empty(), "no trace unless asked for");
        s.keep_trace(2);
        let room = s.trace.capacity();
        for id in 2..10u64 {
            s.record(id, id * 10, id * 10 + 5, true);
        }
        assert_eq!(s.trace.capacity(), room);
        assert_eq!(s.trace.len(), room);
        assert_eq!(s.trace[0], [2, 20, 25]);
        assert_eq!(s.trace[1], [3, 30, 35]);
    }
}

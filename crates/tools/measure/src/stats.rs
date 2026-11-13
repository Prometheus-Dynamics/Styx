//! What the reader measures from each frame: identity (gaps, duplicates, order), capture times
//! (the rate), capture-to-reader latency, and the ISP's time. Each record is a few integer
//! operations and a histogram bump; nothing allocates per frame.

use crate::hist::Histogram;

/// Counts and times over the frames one reader took.
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// Frames the reader handed over.
    pub delivered: u64,
    /// Frames whose id was not the one after the previous id (a skip in the sensor's sequence).
    pub gaps: u64,
    /// The largest run of ids missing at one gap.
    pub largest_gap: u64,
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
    /// Gaps between consecutive capture times (ns).
    pub intervals: Histogram,
    /// Capture to reader, now minus the capture time (ns). Negative values are counted apart.
    pub latency: Histogram,
    pub negative_latency: u64,
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
            last_id: None,
            first_capture_ns: None,
            last_capture_ns: None,
            prev_capture_ns: None,
            intervals: Histogram::new(),
            latency: Histogram::new(),
            negative_latency: 0,
        }
    }

    /// Records one frame: its id, its capture time, the time now (when the reader has it), and
    /// whether its metadata decoded.
    pub fn record(&mut self, id: u64, capture_ns: u64, now_ns: u64, metadata_ok: bool) {
        self.delivered += 1;
        if !metadata_ok {
            self.metadata_refused += 1;
        }
        match self.last_id {
            None => self.first_id = Some(id),
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
        if let Some(prev) = self.prev_capture_ns {
            self.intervals.record(capture_ns.saturating_sub(prev));
        }
        self.prev_capture_ns = Some(capture_ns);
        self.last_capture_ns = Some(capture_ns);
        match now_ns.checked_sub(capture_ns) {
            Some(latency) => self.latency.record(latency),
            None => self.negative_latency += 1,
        }
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counts sensor frames that never reached Styx, from sequence numbers or timestamps.
pub(crate) struct SequenceGapTracker {
    last_sequence: Option<u32>,
    #[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
    last_timestamp: Option<u64>,
    missed: Arc<AtomicU64>,
}

impl SequenceGapTracker {
    pub(crate) fn new(missed: Arc<AtomicU64>) -> Self {
        Self {
            last_sequence: None,
            last_timestamp: None,
            missed,
        }
    }

    /// Forget the last frame: the stream was stopped on purpose and restarts now.
    #[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
    pub(crate) fn restart(&mut self) {
        self.last_sequence = None;
        self.last_timestamp = None;
    }

    fn add(&self, missed: u64) {
        if missed > 0 {
            self.missed.fetch_add(missed, Ordering::Relaxed);
        }
    }

    /// For sequence numbers that count sensor frames (V4L2).
    pub(crate) fn observe(&mut self, sequence: u32) {
        if let Some(last) = self.last_sequence
            && sequence > last
        {
            self.add(u64::from(sequence - last - 1));
        }
        // A lower number means the stream restarted (or wrapped); start over from it.
        self.last_sequence = Some(sequence);
    }

    /// For backends whose sequence numbers count completed requests rather than sensor frames
    /// (libcamera on Raspberry Pi): infer missing frames from the gap between sensor timestamps
    /// and the frame duration the sensor reported for this frame.
    #[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
    pub(crate) fn observe_timestamp(&mut self, timestamp_ns: u64, frame_duration_ns: u64) {
        if let Some(last) = self.last_timestamp
            && frame_duration_ns > 0
            && timestamp_ns > last
        {
            let delta = timestamp_ns - last;
            if delta > frame_duration_ns + frame_duration_ns / 2 {
                self.add((delta + frame_duration_ns / 2) / frame_duration_ns - 1);
            }
        }
        self.last_timestamp = Some(timestamp_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::SequenceGapTracker;

    #[test]
    fn sequence_gaps_count_missing_frames_and_tolerate_restarts() {
        let missed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut tracker = SequenceGapTracker::new(missed.clone());
        for sequence in [10, 11, 14, 15, 0, 1, 3] {
            tracker.observe(sequence);
        }
        // 12, 13 and 2 are missing; the restart at 0 is not a gap.
        assert_eq!(missed.load(std::sync::atomic::Ordering::Relaxed), 3);

        let missed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut tracker = SequenceGapTracker::new(missed.clone());
        let frame = 33_333_333;
        // On time, slightly late (jitter), two frames lost, then 37 lost (a 1.27 s stall).
        for ts in [
            0,
            frame,
            2 * frame + frame / 3,
            5 * frame,
            5 * frame + 1_267_000_000,
        ] {
            tracker.observe_timestamp(ts, frame);
        }
        assert_eq!(missed.load(std::sync::atomic::Ordering::Relaxed), 2 + 37);
    }
}

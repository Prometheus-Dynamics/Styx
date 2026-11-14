//! One control's posts and the values it has put in force: the frame-accurate part.
//!
//! A value for frame `N` is posted during frame `N - delay`. Until then it waits here, as a
//! pending entry. The history keeps `(landing frame, code)` for each post that went to the sensor,
//! in landing order, so the value in force on any recent frame is known.

use std::collections::VecDeque;

/// Requests per control waiting for their post frame. A full queue drops its oldest request.
pub(crate) const PENDING: usize = 16;
/// Posts kept per control for [`Lane::value_at`]. Frames older than the first kept post report
/// the first kept value.
pub(crate) const HISTORY: usize = 64;

/// One control's schedule.
#[derive(Debug)]
pub(crate) struct Lane {
    /// Frames from a write to the first frame it shows on, from the mode.
    pub(crate) delay: u32,
    /// Requested `(frame, code)` not yet posted, by frame.
    pending: Vec<(u64, u32)>,
    /// Posted `(landing frame, code)`, in the order they were posted (landing order).
    history: VecDeque<(u64, u32)>,
    superseded: u64,
    dropped: u64,
}

impl Lane {
    /// A lane whose value `code` is in force from frame 0.
    pub(crate) fn new(delay: u32, code: u32) -> Self {
        let mut history = VecDeque::with_capacity(HISTORY + 1);
        history.push_back((0, code));
        Self {
            delay,
            pending: Vec::with_capacity(PENDING),
            history,
            superseded: 0,
            dropped: 0,
        }
    }

    /// Asks for `code` from frame `frame` on. A request for the same frame replaces the earlier one.
    pub(crate) fn insert(&mut self, frame: u64, code: u32) {
        match self.pending.binary_search_by_key(&frame, |&(f, _)| f) {
            Ok(i) => self.pending[i] = (frame, code),
            Err(_) => {
                if self.pending.len() == PENDING {
                    self.pending.remove(0);
                    self.dropped += 1;
                }
                let i = self.pending.partition_point(|&(f, _)| f < frame);
                self.pending.insert(i, (frame, code));
            }
        }
    }

    /// The post due in frame `at`: every request whose post frame (`frame - delay`) has come. The
    /// latest of them is the one posted; the earlier ones are superseded by it. `None` when nothing
    /// is due.
    pub(crate) fn take_due(&mut self, at: u64) -> Option<(u64, u32)> {
        let limit = at.saturating_add(u64::from(self.delay));
        let due = self.pending.partition_point(|&(frame, _)| frame <= limit);
        if due == 0 {
            return None;
        }
        self.superseded += due as u64 - 1;
        let (frame, code) = self.pending[due - 1];
        self.pending.drain(..due);
        Some((frame, code))
    }

    /// Records a post that landed on `landing`.
    pub(crate) fn record(&mut self, landing: u64, code: u32) {
        self.history.push_back((landing, code));
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    /// True when every post the sensor took for this control has landed by `frame`: the value the
    /// driver holds is then the one in force on `frame`. Posts land in order, so the last one
    /// decides. A lane with no post is settled.
    pub(crate) fn settled_at(&self, frame: u64) -> bool {
        self.history
            .back()
            .is_none_or(|&(landing, _)| landing <= frame)
    }

    /// The code in force on `frame`, from what was posted.
    pub(crate) fn value_at(&self, frame: u64) -> u32 {
        self.history
            .iter()
            .rev()
            .find(|&&(landing, _)| landing <= frame)
            .or_else(|| self.history.front())
            .map_or(0, |&(_, code)| code)
    }

    /// Requests replaced by a later one for the same post frame, or superseded by a later due one.
    pub(crate) fn superseded(&self) -> u64 {
        self.superseded
    }

    /// Requests dropped because the queue was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Requests waiting for their post frame.
    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }
}

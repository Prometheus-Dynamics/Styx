//! Frame-accurate control scheduling (the explicit counterpart of libcamera's DelayedControls).
//!
//! # Model
//!
//! Frames are numbered by sequence. A value written during frame `S` (after its frame start,
//! before the next) first affects frame `S + delay` for that control. With group hold, all
//! writes of one frame are held and launched together, and delays count from that frame.
//!
//! * [`ControlScheduler::request`] asks for values to be in effect from frame `N`. Each control
//!   is scheduled for issue at `N - delay`, and the call returns the frame each value is
//!   predicted to land on (later than `N` when the request came too late).
//! * [`ControlScheduler::frame_start`] is called at every frame start and returns the batch of
//!   writes to issue now. [`ControlScheduler::issue_now`] issues whatever is due without
//!   waiting, e.g. before streaming starts (those values apply from frame 0).
//! * [`ControlScheduler::report`] records values read back for a frame (embedded data);
//!   [`ControlScheduler::applied`] gives the values that produced a frame, reported where
//!   available, predicted otherwise.
//!
//! Values are register codes: exposure in lines × 2^fraction_bits, gains as codes, frame length
//! in lines. Exposures are clamped at issue time to the frame length predicted for the frame
//! they land on.

use crate::desc::Delays;
use crate::fixed::FixedVec;
use crate::frame_map::FrameMap;

/// A scheduled control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Control {
    /// Frame length (VTS) in lines. Ordered first so it is written before exposure.
    #[default]
    FrameLength,
    /// Exposure code.
    Exposure,
    /// Analogue gain code.
    AnalogGain,
    /// Digital gain code.
    DigitalGain,
}

impl Control {
    /// All controls, in write order.
    pub const ALL: [Control; 4] = [
        Control::FrameLength,
        Control::Exposure,
        Control::AnalogGain,
        Control::DigitalGain,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// Values for some or all controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControlSet {
    values: [Option<u32>; 4],
}

impl ControlSet {
    /// An empty set.
    pub const fn new() -> Self {
        Self { values: [None; 4] }
    }

    /// Builder: set a value.
    pub fn with(mut self, control: Control, value: u32) -> Self {
        self.set(control, value);
        self
    }

    /// Set a value.
    pub fn set(&mut self, control: Control, value: u32) {
        self.values[control.index()] = Some(value);
    }

    /// A value, if set.
    pub fn get(&self, control: Control) -> Option<u32> {
        self.values[control.index()]
    }

    /// Remove a value.
    pub fn clear(&mut self, control: Control) {
        self.values[control.index()] = None;
    }

    /// The set values, in write order.
    pub fn iter(&self) -> impl Iterator<Item = (Control, u32)> + '_ {
        Control::ALL
            .into_iter()
            .filter_map(|c| self.get(c).map(|v| (c, v)))
    }

    /// True when nothing is set.
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(Option::is_none)
    }
}

/// Where a requested value is predicted to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Landing {
    /// The control.
    pub control: Control,
    /// The value.
    pub value: u32,
    /// The frame it was requested for.
    pub requested: u64,
    /// The first frame predicted to use it.
    pub frame: u64,
}

impl Landing {
    /// The request came too late to land on the requested frame.
    pub fn late(&self) -> bool {
        self.frame > self.requested
    }
}

/// Writes to issue now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IssueBatch {
    /// The frame during which the batch is issued (`None` before streaming starts).
    pub frame: Option<u64>,
    /// The values to write.
    pub controls: ControlSet,
    /// First frame each written value affects, indexed like [`Control::ALL`].
    pub lands: [Option<u64>; 4],
    /// The exposure originally requested, when it was clamped to the frame length.
    pub exposure_clamped_from: Option<u32>,
}

impl IssueBatch {
    /// First frame the written value of `control` affects.
    pub fn lands(&self, control: Control) -> Option<u64> {
        self.lands[control.index()]
    }
}

/// Values that produced a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The frame.
    pub frame: u64,
    /// The values (every control that has a value).
    pub values: ControlSet,
    /// The subset that was reported (read back) rather than predicted.
    pub reported: ControlSet,
}

/// A reported value that differs from the prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mismatch {
    /// The frame.
    pub frame: u64,
    /// The control.
    pub control: Control,
    /// What the scheduler predicted.
    pub predicted: Option<u32>,
    /// What was reported.
    pub reported: u32,
}

/// Exposure limit used to clamp exposure codes to the frame length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposureLimit {
    /// Minimum lines.
    pub min: u32,
    /// Exposure ≤ frame_length − margin.
    pub margin: u32,
    /// Fractional bits of the exposure code.
    pub fraction_bits: u8,
}

/// Where the values of one request land: one entry per control set, at most four.
pub type Landings = FixedVec<Landing, 4>;

/// Reported values that differ from the prediction: at most one per control.
pub type Mismatches = FixedVec<Mismatch, 4>;

/// Frames of history kept for [`ControlScheduler::applied`].
pub const HISTORY_FRAMES: u64 = 64;

/// Requests per control waiting for their frame (requests for distinct frames not issued yet;
/// a full queue drops its oldest request, [`ControlScheduler::dropped_requests`]).
pub const PENDING_REQUESTS: usize = 16;

/// Issued values and reports kept per control: [`HISTORY_FRAMES`] plus room for values issued
/// ahead (control delays) and the value in effect before the history.
const HISTORY_SLOTS: usize = HISTORY_FRAMES as usize + 32;

/// The control scheduler. See the [module documentation](self).
///
/// It does not allocate: requests, issued values and reports live in fixed rings sorted by
/// frame (about 9 KiB), so the frame path runs without a heap once streaming.
#[derive(Debug, Clone)]
pub struct ControlScheduler {
    delays: [u32; 4],
    limit: Option<ExposureLimit>,
    started: Option<u64>,
    /// Per control: requested frame -> value, not yet issued.
    pending: [FrameMap<u32, PENDING_REQUESTS>; 4],
    /// Per control: first frame -> value, issued.
    committed: [FrameMap<u32, HISTORY_SLOTS>; 4],
    reported: FrameMap<ControlSet, HISTORY_SLOTS>,
}

impl ControlScheduler {
    /// A scheduler whose `initial` values apply from frame 0 (written before streaming).
    pub fn new(delays: Delays, initial: ControlSet) -> Self {
        let mut s = Self {
            delays: [
                delays.frame_length,
                delays.exposure,
                delays.analog_gain,
                delays.digital_gain,
            ],
            limit: None,
            started: None,
            pending: Default::default(),
            committed: Default::default(),
            reported: FrameMap::default(),
        };
        for (c, v) in initial.iter() {
            s.committed[c.index()].insert(0, v);
        }
        s
    }

    /// Clamp exposures to the frame length they land on.
    pub fn with_exposure_limit(mut self, limit: ExposureLimit) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Delay of a control in frames.
    pub fn delay(&self, control: Control) -> u32 {
        self.delays[control.index()]
    }

    /// The largest delay: how far ahead requests should be made.
    pub fn max_delay(&self) -> u32 {
        self.delays.iter().copied().max().unwrap_or(0)
    }

    /// The last frame start seen.
    pub fn current_frame(&self) -> Option<u64> {
        self.started
    }

    /// Requests dropped because a control had [`PENDING_REQUESTS`] requests for later frames
    /// waiting (the oldest goes). Zero in normal use: requests are issued at most a few frames
    /// ahead.
    pub fn dropped_requests(&self) -> u64 {
        self.pending.iter().map(FrameMap::dropped).sum()
    }

    /// Whether requested values wait for a later frame start to be written (a caller that
    /// drives frame starts itself must then wait for them).
    pub fn has_pending(&self) -> bool {
        self.pending.iter().any(|p| !p.is_empty())
    }

    /// Predicted landing frame for a value requested for `target`, if not issued early.
    fn landing(&self, control: Control, target: u64) -> u64 {
        let d = u64::from(self.delay(control));
        match self.started {
            None if target < d => 0,
            None => target,
            // Issued at the next frame start at the earliest.
            Some(s) => target.max(s + 1 + d),
        }
    }

    /// Ask for values to be in effect from frame `frame`. Returns where each value is predicted
    /// to land. A later request for the same control and frame replaces an earlier one.
    pub fn request(&mut self, frame: u64, controls: &ControlSet) -> Landings {
        controls
            .iter()
            .map(|(c, v)| {
                self.pending[c.index()].insert(frame, v);
                Landing {
                    control: c,
                    value: v,
                    requested: frame,
                    frame: self.landing(c, frame),
                }
            })
            .collect()
    }

    /// Frame `seq` has started: returns the writes to issue during this frame.
    pub fn frame_start(&mut self, seq: u64) -> IssueBatch {
        self.started = Some(seq);
        let batch = self.take(Some(seq));
        if seq > HISTORY_FRAMES {
            self.prune(seq - HISTORY_FRAMES);
        }
        batch
    }

    /// Like [`Self::request`], but writes due in the current frame (the last one started) are
    /// taken at once instead of at the next frame start: the returned batch must be written
    /// before the current frame ends (before the sensor latches its registers for the next
    /// one). Values then land `delay` frames after the current frame, so a request for
    /// `current + delay` is on time. Before streaming this is [`Self::issue_now`] after the
    /// request.
    pub fn request_now(&mut self, frame: u64, controls: &ControlSet) -> (Landings, IssueBatch) {
        for (c, v) in controls.iter() {
            self.pending[c.index()].insert(frame, v);
        }
        let batch = self.take(self.started);
        let landings = controls
            .iter()
            .map(|(c, v)| Landing {
                control: c,
                value: v,
                requested: frame,
                frame: match (batch.lands(c), self.started) {
                    (Some(l), _) => l,
                    (None, Some(s)) => frame.max(s + 1 + u64::from(self.delay(c))),
                    (None, None) => self.landing(c, frame),
                },
            })
            .collect();
        (landings, batch)
    }

    /// Issue what is due without waiting for the next frame start: before streaming, values
    /// requested for frames earlier than their delay (they then apply from frame 0); while
    /// streaming, values due in the current frame (the caller must write them before the frame
    /// ends).
    pub fn issue_now(&mut self) -> IssueBatch {
        self.take(self.started)
    }

    fn take(&mut self, at: Option<u64>) -> IssueBatch {
        let mut batch = IssueBatch {
            frame: at,
            ..IssueBatch::default()
        };
        for c in Control::ALL {
            let i = c.index();
            let d = u64::from(self.delays[i]);
            // Due: requested frame <= at + delay (pre-start: < delay).
            let due_limit = match at {
                Some(s) => s + d,
                None if d == 0 => continue,
                None => d - 1,
            };
            // Of several due values the one for the latest frame wins; the others would land on
            // the same frame and be overwritten.
            let Some(mut value) = self.pending[i].take_up_to(due_limit) else {
                continue;
            };
            let lands = at.map_or(0, |s| s + d);
            if c == Control::Exposure
                && let Some(clamped) = self.clamp_exposure(value, lands)
            {
                batch.exposure_clamped_from = Some(value);
                value = clamped;
            }
            self.committed[i].insert(lands, value);
            batch.controls.set(c, value);
            batch.lands[i] = Some(lands);
        }
        batch
    }

    fn clamp_exposure(&self, code: u32, lands: u64) -> Option<u32> {
        let limit = self.limit?;
        let fl = self.predicted_value(Control::FrameLength, lands)?;
        let scale = 1u32 << limit.fraction_bits;
        let max = fl.saturating_sub(limit.margin).saturating_mul(scale);
        let min = limit.min.saturating_mul(scale);
        let clamped = code.clamp(min, max.max(min));
        (clamped != code).then_some(clamped)
    }

    fn predicted_value(&self, c: Control, frame: u64) -> Option<u32> {
        let i = c.index();
        // Candidates as (first frame, priority, value); the latest wins. At the same frame an
        // issued value beats a report (the write is assumed to have landed after it was read
        // back), and a pending request beats both.
        let committed = self.committed[i]
            .last_at_or_before(frame)
            .map(|(f, v)| (f, 1, v));
        let reported = self
            .reported
            .up_to(frame)
            .rev()
            .find_map(|(f, set)| set.get(c).map(|v| (f, 0, v)));
        let pending = self.pending[i]
            .iter()
            .map(|(t, v)| (self.landing(c, t), t, *v))
            .filter(|(l, _, _)| *l <= frame)
            .max_by_key(|(l, t, _)| (*l, *t))
            .map(|(l, _, v)| (l, 2, v));
        [committed, reported, pending]
            .into_iter()
            .flatten()
            .max_by_key(|(f, p, _)| (*f, *p))
            .map(|(_, _, v)| v)
    }

    /// The values predicted for a frame (issued and pending requests).
    pub fn predicted(&self, frame: u64) -> ControlSet {
        let mut set = ControlSet::new();
        for c in Control::ALL {
            if let Some(v) = self.predicted_value(c, frame) {
                set.set(c, v);
            }
        }
        set
    }

    /// Record values read back for a frame (e.g. from embedded data). Returns the values that
    /// differ from the prediction. A reported value also predicts later frames until a value
    /// issued to land after the reported frame takes over.
    pub fn report(&mut self, frame: u64, values: &ControlSet) -> Mismatches {
        let mut mismatches = Mismatches::new();
        for (c, v) in values.iter() {
            let predicted = self.predicted_value(c, frame);
            if predicted != Some(v) {
                // At most one per control: never full.
                let _ = mismatches.push(Mismatch {
                    frame,
                    control: c,
                    predicted,
                    reported: v,
                });
            }
        }
        let entry = self.reported.entry(frame);
        for (c, v) in values.iter() {
            entry.set(c, v);
        }
        mismatches
    }

    /// The values that produced a frame: reported where available, predicted otherwise.
    pub fn applied(&self, frame: u64) -> Applied {
        let reported = self.reported.get(frame).copied().unwrap_or_default();
        let mut values = self.predicted(frame);
        for (c, v) in reported.iter() {
            values.set(c, v);
        }
        Applied {
            frame,
            values,
            reported,
        }
    }

    /// Forget history before `frame` (the value in effect at `frame` is kept).
    pub fn prune(&mut self, frame: u64) {
        for map in &mut self.committed {
            let in_effect = map.last_at_or_before(frame);
            map.remove_before(frame);
            if let Some((_, v)) = in_effect
                && map.get(frame).is_none()
            {
                map.insert(frame, v);
            }
        }
        self.reported.remove_before(frame);
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;

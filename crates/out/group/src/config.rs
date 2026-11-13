//! Settings of a [`Grouper`](crate::Grouper).

/// What a group needs to be emitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GroupPolicy {
    /// Every registered camera (connected or not) contributes a frame, or the anchor is dropped.
    /// Groups are queued in order ([`GroupConfig::output_depth`]).
    #[default]
    Strict,
    /// Best effort: wait up to [`GroupConfig::deadline_ns`] for the connected cameras, then emit
    /// the frames that matched (at least [`GroupConfig::min_members`]; disconnected cameras are
    /// not waited for). [`FrameGroup::complete`](crate::FrameGroup::complete) tells full groups
    /// apart.
    Partial,
    /// The newest group of every connected camera: a newer group replaces one not taken yet
    /// (`Stale`), so the consumer always gets the freshest synchronized set.
    Latest,
}

impl GroupPolicy {
    /// Stable snake-case name (metric labels, logs).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Partial => "partial",
            Self::Latest => "latest",
        }
    }
}

/// How cameras at different frame rates pair up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RateMatch {
    /// Anchor on the oldest pending frame of any camera, pair to the nearest frames. Right for
    /// cameras at one rate; at different rates the faster camera's extra frames find no match.
    #[default]
    Nearest,
    /// Anchor on the slowest camera (largest measured frame period): one group per slowest
    /// frame, the faster cameras' frames in between dropped as `Unmatched`.
    Slowest,
}

/// Settings of a [`Grouper`](crate::Grouper). Every field has a default from [`GroupConfig::new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupConfig {
    /// Largest distance from the anchor's timestamp a member may have. About half the frame
    /// period for free-running cameras; tens of microseconds for hardware-synchronized ones.
    pub tolerance_ns: u64,
    pub policy: GroupPolicy,
    pub rate: RateMatch,
    /// How long (after the anchor frame arrived) a group waits for a camera whose frame is not
    /// there yet: then `Partial` emits without it, `Strict` and `Latest` drop the anchor.
    pub deadline_ns: u64,
    /// Frames held per camera (at least 1); a camera with this many pending loses its oldest.
    pub depth: usize,
    /// Groups held until taken (at least 1); `Latest` holds one.
    pub output_depth: usize,
    /// `Partial` and `Latest`: fewest frames a group may have (clamped to the cameras there).
    pub min_members: usize,
}

impl GroupConfig {
    /// `tolerance_ns` with the defaults: strict, nearest, 100 ms deadline, 3 frames per camera,
    /// 2 groups queued, groups of at least 2.
    pub const fn new(tolerance_ns: u64) -> Self {
        Self {
            tolerance_ns,
            policy: GroupPolicy::Strict,
            rate: RateMatch::Nearest,
            deadline_ns: 100_000_000,
            depth: 3,
            output_depth: 2,
            min_members: 2,
        }
    }

    pub const fn policy(mut self, policy: GroupPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub const fn rate(mut self, rate: RateMatch) -> Self {
        self.rate = rate;
        self
    }

    pub const fn deadline_ns(mut self, deadline_ns: u64) -> Self {
        self.deadline_ns = deadline_ns;
        self
    }

    pub const fn depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub const fn output_depth(mut self, output_depth: usize) -> Self {
        self.output_depth = output_depth;
        self
    }

    pub const fn min_members(mut self, min_members: usize) -> Self {
        self.min_members = min_members;
        self
    }
}

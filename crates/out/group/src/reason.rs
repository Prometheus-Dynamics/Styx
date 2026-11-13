//! Why a frame left a group without being handed out in one.

/// Why a frame left a [`Grouper`](crate::Grouper) without being in a group handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Another camera has frames, but none within the tolerance of this one.
    Unmatched,
    /// A frame of the same camera nearer to the group was used instead, or an older frame was
    /// passed over by the one chosen.
    Superseded,
    /// A camera needed for the group sent nothing in time (deadline) or is disconnected.
    Incomplete,
    /// The camera's pending frames were full ([`GroupConfig::depth`](crate::GroupConfig::depth)):
    /// the oldest went.
    Overflow,
    /// The frame arrived after groups past its time were emitted (or went back in time).
    Late,
    /// In a group nobody took before a newer one replaced it (or the output was full).
    Stale,
    /// Its timestamp cannot be put on the grouper's clock (unknown or unrelated clock).
    Clock,
    /// Pending when its camera disconnected or was removed.
    Disconnected,
}

impl DropReason {
    /// How many reasons there are.
    pub const COUNT: usize = 8;

    /// Every reason, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Unmatched,
        Self::Superseded,
        Self::Incomplete,
        Self::Overflow,
        Self::Late,
        Self::Stale,
        Self::Clock,
        Self::Disconnected,
    ];

    /// Position in [`Self::ALL`] (and in the per-reason counters).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable snake-case name, used as a metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmatched => "unmatched",
            Self::Superseded => "superseded",
            Self::Incomplete => "incomplete",
            Self::Overflow => "overflow",
            Self::Late => "late",
            Self::Stale => "stale",
            Self::Clock => "clock",
            Self::Disconnected => "disconnected",
        }
    }
}

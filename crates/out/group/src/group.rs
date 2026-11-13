//! A group: one frame per camera, held in slots indexed by camera.

use core::array;

/// A camera's index in a [`Grouper`](crate::Grouper): the order `add_camera` gave it. Stable for
/// the grouper's life (a removed camera's index is not reused).
pub type CameraIndex = usize;

/// One camera's frame in a [`FrameGroup`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member<T> {
    /// On the grouper's clock (CLOCK_MONOTONIC nanoseconds for Styx frames).
    pub timestamp_ns: u64,
    pub item: T,
}

/// Frames of several cameras taken at (about) the same time. Holds at most one member per
/// camera, in a fixed array of `N` slots, so a group is built and passed on without allocating.
#[derive(Debug)]
pub struct FrameGroup<T, const N: usize> {
    /// Groups handed out before this one by its grouper (0 first), gaps where groups went stale.
    pub sequence: u64,
    /// The anchor frame's timestamp: what the members were matched to.
    pub reference_ns: u64,
    /// Latest minus earliest member timestamp.
    pub spread_ns: u64,
    /// Every registered camera is in it.
    pub complete: bool,
    slots: [Option<Member<T>>; N],
}

impl<T, const N: usize> FrameGroup<T, N> {
    pub(crate) fn from_slots(
        sequence: u64,
        reference_ns: u64,
        complete: bool,
        slots: [Option<Member<T>>; N],
    ) -> Self {
        let (min, max) = slots.iter().flatten().fold((u64::MAX, 0), |(lo, hi), m| {
            (lo.min(m.timestamp_ns), hi.max(m.timestamp_ns))
        });
        let spread_ns = max.saturating_sub(min);
        Self {
            sequence,
            reference_ns,
            spread_ns,
            complete,
            slots,
        }
    }

    /// `camera`'s member.
    pub fn member(&self, camera: CameraIndex) -> Option<&Member<T>> {
        self.slots.get(camera)?.as_ref()
    }

    /// `camera`'s frame.
    pub fn get(&self, camera: CameraIndex) -> Option<&T> {
        self.member(camera).map(|m| &m.item)
    }

    /// Takes `camera`'s frame out of the group.
    pub fn take(&mut self, camera: CameraIndex) -> Option<T> {
        self.slots.get_mut(camera)?.take().map(|m| m.item)
    }

    /// `camera`'s timestamp minus the reference.
    pub fn offset_ns(&self, camera: CameraIndex) -> Option<i64> {
        self.member(camera)
            .map(|m| signed_diff(m.timestamp_ns, self.reference_ns))
    }

    /// Members in the group.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Cameras of the group, in order.
    pub fn cameras(&self) -> impl Iterator<Item = CameraIndex> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(camera, s)| s.as_ref().map(|_| camera))
    }

    /// The same group over other items (e.g. frames turned into graph payloads).
    pub fn map<U>(self, mut f: impl FnMut(CameraIndex, T) -> U) -> FrameGroup<U, N> {
        let mut slots: [Option<Member<U>>; N] = array::from_fn(|_| None);
        for (camera, slot) in self.slots.into_iter().enumerate() {
            slots[camera] = slot.map(|m| Member {
                timestamp_ns: m.timestamp_ns,
                item: f(camera, m.item),
            });
        }
        FrameGroup {
            sequence: self.sequence,
            reference_ns: self.reference_ns,
            spread_ns: self.spread_ns,
            complete: self.complete,
            slots,
        }
    }
}

pub(crate) fn signed_diff(a: u64, b: u64) -> i64 {
    (i128::from(a) - i128::from(b)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

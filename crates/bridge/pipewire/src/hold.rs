//! When a frame the node sent to PipeWire may be given back to the producer.
//!
//! PipeWire has no signal for "the consumer is done with this buffer" on an ordinary link. What
//! it does (read from its source, 1.0.5 and 1.6.8):
//!
//! - A sent buffer's id is written to an io area of the link. A link has one area, or two that
//!   alternate with the graph cycle's low bit (a consumer that runs a cycle behind reads the one
//!   written the cycle before).
//! - The id stays in its area, where the consumer can take it, until the node's next send to the
//!   same area replaces it. Areas are written only in cycles where the node sends, so that can be
//!   any number of cycles later.
//! - A consumer that takes a buffer counts itself on the buffer's busy count until it gives the
//!   buffer back, and clears the area: it cannot take the same send twice.
//! - PipeWire hands the buffer back to the node one cycle after the send, taken or not. That
//!   return says nothing about the consumer. Its node-to-node "reuse buffer" call is not carried
//!   across a link (a stub in `impl-port.c`, `-ENOTSUP` in the client transport), and its own
//!   v4l2 source recycles on the same return.
//!
//! So a buffer is free when PipeWire has given it back, no consumer is counted on it, and either
//! a consumer was seen to take it as many times as it was sent (each send can be taken once), or
//! later sends have replaced every send of it in its area and [`SETTLE_CYCLES`] have passed since
//! (a consumer that read the area just before the replacement has counted itself by then).
//!
//! The node does not always know which area a send went to (it learns the cycle of a send from
//! PipeWire's time snapshot, and its thread can be late). Such a send replaces nothing, and is
//! itself replaced only when both areas have been written by later sends whose areas are known.
//! When the frames the node holds leave the producer nothing to write to, the node sends again
//! the oldest frame it still holds ([`Holds::oldest_held`]): the send replaces what was in its
//! area, and a consumer that is running takes it. That is an older whole frame for the consumer,
//! or one it has had before; never a torn one.
//!
//! This module is the rule alone, with no PipeWire in it, so that its model test can try every
//! short history.

/// How many of the node's graph cycles pass between a buffer being replaced in its area and the
/// node trusting the buffer's busy count to show a consumer that took it just before.
pub(crate) const SETTLE_CYCLES: u64 = 2;

/// Which of a link's io areas a send was written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    Known(u8),
    /// The node could not tell: either area.
    Unknown,
}

/// How many unresolved sends of one buffer the node keeps track of. A buffer is sent again only
/// by [`Holds::oldest_held`], and not once it has this many.
const MAX_SENDS: usize = 4;

/// One send of a buffer that may still be in an area.
#[derive(Debug, Clone, Copy)]
struct Sent {
    /// Its order among all sends.
    seq: u64,
    /// `None` until it is seen to have gone out.
    area: Option<Area>,
    /// The node's cycle count when it first knew a later send had replaced this one in its area.
    replaced: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Out {
    /// The sends of this buffer that may still be in an area, oldest first. One that was replaced
    /// [`SETTLE_CYCLES`] ago is dropped from the list.
    sends: [Sent; MAX_SENDS],
    count: usize,
    /// PipeWire has given the buffer back since the newest send.
    back: bool,
    /// How many times a consumer was seen to become counted on the buffer: each is one send
    /// taken, though not which. A take and return between two looks is not seen, which only
    /// keeps the buffer longer. When a replaced send is dropped, one take is dropped with it: it
    /// may have been the take of that send.
    takes: usize,
    /// How many consumers were counted on the buffer at the last look.
    was_on: u32,
}

/// What the node holds in each ring buffer.
#[derive(Debug, Clone)]
pub(crate) struct Holds {
    slots: Vec<Option<Out>>,
    sends: u64,
    /// The newest send known to have been written to each area, 0 for none.
    latest: [u64; 2],
}

impl Holds {
    pub(crate) fn new(buffers: usize) -> Self {
        Self {
            slots: vec![None; buffers],
            sends: 0,
            latest: [0; 2],
        }
    }

    /// Buffer `index` is filled with a frame and is about to be sent, or (a buffer that still
    /// holds its frame) sent again.
    pub(crate) fn send(&mut self, index: usize) {
        self.sends += 1;
        let empty = Sent {
            seq: 0,
            area: None,
            replaced: None,
        };
        let mut out = self.slots[index].unwrap_or(Out {
            sends: [empty; MAX_SENDS],
            count: 0,
            back: false,
            takes: 0,
            was_on: 0,
        });
        debug_assert!(out.count < MAX_SENDS);
        out.sends[out.count] = Sent {
            seq: self.sends,
            ..empty
        };
        out.count += 1;
        out.back = false;
        out.was_on = 0;
        self.slots[index] = Some(out);
    }

    /// The newest send of buffer `index` went out, to `area`.
    pub(crate) fn went_out(&mut self, index: usize, area: Area) {
        let Some(out) = self.slots[index].as_mut() else {
            return;
        };
        let sent = &mut out.sends[out.count - 1];
        sent.area = Some(area);
        if let Area::Known(area) = area {
            let latest = &mut self.latest[usize::from(area & 1)];
            *latest = (*latest).max(sent.seq);
        }
    }

    /// PipeWire gave buffer `index` back.
    pub(crate) fn came_back(&mut self, index: usize) {
        if let Some(out) = self.slots.get_mut(index).and_then(Option::as_mut) {
            out.back = true;
        }
    }

    /// Whether the node has buffer `index` back from PipeWire (and so is itself counted on it).
    pub(crate) fn is_back(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .is_some_and(|slot| slot.is_some_and(|out| out.back))
    }

    /// Whether buffer `index` holds a frame.
    pub(crate) fn is_held(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(Option::is_some)
    }

    /// Buffer `index` is PipeWire's to free: whatever it held is forgotten.
    pub(crate) fn forget(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = None;
        }
    }

    /// Whether a later send has been written to the area `sent` is in.
    fn replaced(&self, sent: &Sent) -> bool {
        match sent.area {
            None => false,
            Some(Area::Known(area)) => self.latest[usize::from(area & 1)] > sent.seq,
            Some(Area::Unknown) => self.latest[0] > sent.seq && self.latest[1] > sent.seq,
        }
    }

    fn flying(out: &Out) -> bool {
        out.sends[out.count - 1].area.is_none()
    }

    /// Frees what can be freed at the node's cycle count `cycle`, given how many consumers are
    /// counted on each buffer now, and calls `freed` with each.
    pub(crate) fn settle(
        &mut self,
        cycle: u64,
        consumers_on: impl Fn(usize) -> u32,
        mut freed: impl FnMut(usize),
    ) {
        for index in 0..self.slots.len() {
            let Some(mut out) = self.slots[index] else {
                continue;
            };
            if Self::flying(&out) {
                continue;
            }
            let on = consumers_on(index);
            out.takes += on.saturating_sub(out.was_on) as usize;
            out.was_on = on;
            // Drop each send that was replaced long enough ago: it can no longer be taken.
            let mut kept = 0;
            for at in 0..out.count {
                let mut sent = out.sends[at];
                if sent.replaced.is_none() && self.replaced(&sent) {
                    sent.replaced = Some(cycle);
                }
                if sent
                    .replaced
                    .is_some_and(|since| cycle >= since + SETTLE_CYCLES)
                {
                    out.takes = out.takes.saturating_sub(1);
                } else {
                    out.sends[kept] = sent;
                    kept += 1;
                }
            }
            out.count = kept;
            if out.back && on == 0 && out.takes >= out.count {
                self.slots[index] = None;
                freed(index);
            } else if out.count == 0 {
                // Nothing of it is on the link, and it is not back or a consumer still has it:
                // keep the buffer with a send that is already resolved.
                out.sends[0] = Sent {
                    seq: 0,
                    area: Some(Area::Known(0)),
                    replaced: Some(cycle),
                };
                out.count = 1;
                out.takes = 1;
                self.slots[index] = Some(out);
            } else {
                self.slots[index] = Some(out);
            }
        }
    }

    /// The buffer to send again: the one whose newest send is oldest, among those that have gone
    /// out and come back, that no consumer is counted on, and that waiting will not free (not
    /// every send of it taken, and not every one replaced). `None` when there is none, or it has been sent too
    /// many times to keep track of.
    pub(crate) fn oldest_held(&self, consumer_on: impl Fn(usize) -> bool) -> Option<usize> {
        let mut oldest: Option<(u64, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(out) = slot else {
                continue;
            };
            if Self::flying(out)
                || !out.back
                || out.takes >= out.count
                || out.sends[..out.count]
                    .iter()
                    .all(|sent| self.replaced(sent))
                || out.count >= MAX_SENDS
                || consumer_on(index)
            {
                continue;
            }
            let seq = out.sends[out.count - 1].seq;
            if oldest.is_none_or(|(least, _)| seq < least) {
                oldest = Some((seq, index));
            }
        }
        oldest.map(|(_, index)| index)
    }

    /// How many buffers hold a frame.
    pub(crate) fn held(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Forgets every frame, for a new stream connection.
    pub(crate) fn clear(&mut self, buffers: usize) {
        *self = Self::new(buffers);
    }
}

#[cfg(test)]
mod model {
    //! Every short history of a two- or three-buffer ring on a link with one or two areas, with
    //! the node knowing or not knowing each send's area: in no state may the producer write a
    //! buffer that a consumer holds or can still take.

    use super::*;

    fn settled(holds: &mut Holds, cycle: u64, on: impl Fn(usize) -> bool) -> Vec<usize> {
        let mut freed = Vec::new();
        holds.settle(
            cycle,
            |index| u32::from(on(index)),
            |index| freed.push(index),
        );
        freed
    }

    /// The world as PipeWire and a consumer have it, which the node cannot see.
    #[derive(Clone)]
    struct World {
        areas: usize,
        /// The buffer whose id is in each area, takeable.
        area: [Option<usize>; 2],
        /// How many times the node sent a frame again.
        resent: u64,
        /// How many times a buffer was freed.
        freed: u64,
        /// Buffers a consumer has taken and not given back.
        held: Vec<bool>,
        /// Of those, the ones whose busy count the node can already see. A take shows in the
        /// count a moment after the area is read: here, from the next graph cycle.
        counted: Vec<bool>,
        /// Per buffer: sent and waiting for PipeWire's return a cycle later.
        returning: Vec<bool>,
        /// Buffers the node may fill.
        free: Vec<bool>,
        cycle: u64,
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// The node sends the lowest free buffer; `true` when it learns the area.
        Send(bool),
        /// The consumer takes what is in the area (0 or 1).
        Take(u8),
        /// The consumer gives back the buffer (by index) it holds.
        Give(u8),
        /// A graph cycle passes with no send.
        Idle,
        /// The node sends its oldest held frame again; `true` when it learns the area.
        Again(bool),
    }

    const STEPS: [Step; 10] = [
        Step::Send(true),
        Step::Send(false),
        Step::Take(0),
        Step::Take(1),
        Step::Give(0),
        Step::Give(1),
        Step::Give(2),
        Step::Idle,
        Step::Again(true),
        Step::Again(false),
    ];

    fn explore(
        holds: &Holds,
        world: &World,
        depth: usize,
        trail: &mut Vec<Step>,
        seen: &mut [u64; 3],
    ) {
        // The safety property, in every state: a free buffer is not held and not in an area.
        for (index, &free) in world.free.iter().enumerate() {
            if free {
                assert!(
                    !world.held[index],
                    "buffer {index} is free while a consumer holds it: {trail:?}"
                );
                assert!(
                    !world.area.contains(&Some(index)),
                    "buffer {index} is free while a consumer can still take it: {trail:?}"
                );
            }
        }
        seen[0] += 1;
        if depth == 0 {
            // Tallied at the leaves: histories in which a buffer was freed, or a frame sent again.
            seen[1] += u64::from(world.freed > 0);
            seen[2] += u64::from(world.resent > 0);
            return;
        }
        for &step in &STEPS {
            let mut holds = holds.clone();
            let mut world = world.clone();
            match step {
                Step::Send(known) | Step::Again(known) => {
                    // A cycle in which the node sends: what was sent last cycle comes back.
                    world.cycle += 1;
                    world.counted.clone_from(&world.held);
                    for index in 0..world.returning.len() {
                        if std::mem::take(&mut world.returning[index]) {
                            holds.came_back(index);
                        }
                    }
                    let index = if matches!(step, Step::Send(_)) {
                        match world.free.iter().position(|&f| f) {
                            Some(index) => index,
                            None => continue,
                        }
                    } else {
                        match holds.oldest_held(|i| world.counted[i]) {
                            Some(index) => {
                                world.resent += 1;
                                index
                            }
                            None => continue,
                        }
                    };
                    world.free[index] = false;
                    holds.send(index);
                    let area = (world.cycle as usize) % world.areas;
                    world.area[area] = Some(index);
                    world.returning[index] = true;
                    holds.went_out(
                        index,
                        if known {
                            Area::Known(area as u8)
                        } else {
                            Area::Unknown
                        },
                    );
                }
                Step::Take(area) => {
                    let area = usize::from(area);
                    if area >= world.areas {
                        continue;
                    }
                    let Some(index) = world.area[area].take() else {
                        continue;
                    };
                    world.held[index] = true;
                    world.counted[index] = false;
                }
                Step::Give(index) => {
                    let index = usize::from(index);
                    if index >= world.held.len() || !world.held[index] {
                        continue;
                    }
                    world.held[index] = false;
                    world.counted[index] = false;
                }
                Step::Idle => {
                    world.cycle += 1;
                    world.counted.clone_from(&world.held);
                    for index in 0..world.returning.len() {
                        if std::mem::take(&mut world.returning[index]) {
                            holds.came_back(index);
                        }
                    }
                }
            }
            // The node looks: every tick it reads the busy counts and settles.
            let counted = world.counted.clone();
            holds.settle(
                world.cycle,
                |i| u32::from(counted[i]),
                |index| {
                    world.free[index] = true;
                    world.freed += 1;
                },
            );
            trail.push(step);
            explore(&holds, &world, depth - 1, trail, seen);
            trail.pop();
        }
    }

    fn run(buffers: usize, areas: usize, depth: usize) -> [u64; 3] {
        let world = World {
            areas,
            area: [None; 2],
            held: vec![false; buffers],
            counted: vec![false; buffers],
            returning: vec![false; buffers],
            free: vec![true; buffers],
            cycle: 0,
            resent: 0,
            freed: 0,
        };
        let mut seen = [0; 3];
        explore(
            &Holds::new(buffers),
            &world,
            depth,
            &mut Vec::new(),
            &mut seen,
        );
        seen
    }

    #[test]
    fn no_history_frees_a_buffer_a_consumer_holds_or_can_take() {
        let mut total = [0_u64; 3];
        for (buffers, depth) in [(2, 9), (3, 8)] {
            for areas in [1, 2] {
                let seen = run(buffers, areas, depth);
                // The rule is not safe by never freeing: buffers are freed, and frames sent again,
                // in many of the histories.
                assert!(
                    seen[1] > 0 && seen[2] > 0,
                    "{buffers} buffers, {areas} areas: {seen:?}"
                );
                for (sum, n) in total.iter_mut().zip(seen) {
                    *sum += n;
                }
            }
        }
        println!(
            "model: {} states, none unsafe; a buffer freed in {} histories, a frame sent again in {}",
            total[0], total[1], total[2]
        );
    }

    /// The rule the model would also pass if nothing were ever freed: it does free.
    #[test]
    fn a_taken_and_returned_buffer_is_free_and_a_replaced_one_after_it_settles() {
        let mut holds = Holds::new(3);
        holds.send(0);
        holds.went_out(0, Area::Known(0));
        holds.came_back(0);
        assert!(
            settled(&mut holds, 1, |_| false).is_empty(),
            "the newest in its area"
        );
        // Taken: seen counted, then not.
        assert!(settled(&mut holds, 2, |i| i == 0).is_empty());
        assert_eq!(settled(&mut holds, 3, |_| false), [0]);

        let mut holds = Holds::new(3);
        holds.send(0);
        holds.went_out(0, Area::Known(0));
        holds.came_back(0);
        holds.send(1);
        holds.went_out(1, Area::Known(1));
        assert!(
            settled(&mut holds, 5, |_| false).is_empty(),
            "the other area's send"
        );
        holds.send(2);
        holds.went_out(2, Area::Known(0));
        assert!(
            settled(&mut holds, 6, |_| false).is_empty(),
            "replaced, not settled"
        );
        assert!(settled(&mut holds, 7, |_| false).is_empty());
        assert_eq!(settled(&mut holds, 6 + SETTLE_CYCLES, |_| false), [0]);
    }

    #[test]
    fn a_send_to_an_unknown_area_replaces_nothing_and_needs_both_areas_replaced() {
        let mut holds = Holds::new(4);
        holds.send(0);
        holds.went_out(0, Area::Known(0));
        holds.came_back(0);
        holds.send(1);
        holds.went_out(1, Area::Unknown);
        holds.came_back(1);
        assert!(settled(&mut holds, 10, |_| false).is_empty());
        holds.send(2);
        holds.went_out(2, Area::Known(0));
        holds.came_back(2);
        // Buffer 0 is replaced; buffer 1 has only area 0 replaced.
        assert_eq!(settled(&mut holds, 20, |_| false), []);
        assert_eq!(settled(&mut holds, 20 + SETTLE_CYCLES, |_| false), [0]);
        holds.send(3);
        holds.went_out(3, Area::Known(1));
        assert_eq!(settled(&mut holds, 30, |_| false), []);
        assert_eq!(settled(&mut holds, 30 + SETTLE_CYCLES, |_| false), [1]);
    }

    #[test]
    fn the_oldest_held_frame_is_the_one_to_send_again() {
        let mut holds = Holds::new(2);
        for index in 0..2 {
            holds.send(index);
            holds.went_out(index, Area::Unknown);
            holds.came_back(index);
        }
        assert!(settled(&mut holds, 5, |_| false).is_empty());
        assert_eq!(holds.oldest_held(|_| false), Some(0));
        // Not one a consumer is on: that will free it.
        assert_eq!(holds.oldest_held(|i| i == 0), Some(1));
        holds.send(0);
        assert_eq!(
            holds.oldest_held(|_| false),
            Some(1),
            "buffer 0 is in flight"
        );
    }

    /// A frame sent twice is free once it was taken twice: each send can be taken once.
    #[test]
    fn a_frame_sent_twice_is_free_when_it_was_taken_twice() {
        let mut holds = Holds::new(1);
        holds.send(0);
        holds.went_out(0, Area::Known(0));
        holds.came_back(0);
        holds.send(0);
        holds.went_out(0, Area::Known(1));
        holds.came_back(0);
        assert!(
            settled(&mut holds, 1, |_| true).is_empty(),
            "taken once, held"
        );
        assert!(
            settled(&mut holds, 2, |_| false).is_empty(),
            "one send still on the link"
        );
        assert!(
            settled(&mut holds, 3, |_| true).is_empty(),
            "taken again, held"
        );
        assert_eq!(settled(&mut holds, 4, |_| false), [0]);
    }
}

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
//! a consumer was seen counted on it since the send (it took it, and cannot again), or a later
//! send has replaced it in its area and [`SETTLE_CYCLES`] have passed since (a consumer that read
//! the area just before the replacement has counted itself by then).
//!
//! The node does not always know which area a send went to (it learns the cycle of a send from
//! PipeWire's time snapshot, and its thread can be late). Such a send replaces nothing, and is
//! itself replaced only when both areas have been written by later sends whose areas are known.
//! If that leaves nothing free and nothing that waiting will free, the node sends again a frame
//! it still holds ([`Holds::stuck`]): an older whole frame for the consumer, never a torn one.
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
/// by [`Holds::stuck`], and not once it has this many.
const MAX_SENDS: usize = 4;

#[derive(Debug, Clone, Copy)]
struct Out {
    /// Each send of this buffer that may still be in an area: its order among all sends, and
    /// the area, `None` for the newest until it is seen to have gone out.
    sends: [(u64, Option<Area>); MAX_SENDS],
    count: usize,
    /// PipeWire has given the buffer back since the newest send.
    back: bool,
    /// A consumer was seen counted on the buffer, and the buffer has been sent once only: the
    /// consumer took that send. With more than one send it is not known which was taken.
    taken: bool,
    /// The node's cycle count when it first knew every send was replaced in its area.
    replaced: Option<u64>,
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
        let mut out = self.slots[index].unwrap_or(Out {
            sends: [(0, None); MAX_SENDS],
            count: 0,
            back: false,
            taken: false,
            replaced: None,
        });
        debug_assert!(out.count < MAX_SENDS);
        out.sends[out.count] = (self.sends, None);
        out.count += 1;
        out.back = false;
        out.taken = false;
        out.replaced = None;
        self.slots[index] = Some(out);
    }

    /// The newest send of buffer `index` went out, to `area`.
    pub(crate) fn went_out(&mut self, index: usize, area: Area) {
        let Some(out) = self.slots[index].as_mut() else {
            return;
        };
        let (seq, at) = &mut out.sends[out.count - 1];
        *at = Some(area);
        if let Area::Known(area) = area {
            let latest = &mut self.latest[usize::from(area & 1)];
            *latest = (*latest).max(*seq);
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

    /// Whether every send of `out` has gone out and been replaced in its area.
    fn replaced(&self, out: &Out) -> bool {
        out.sends[..out.count]
            .iter()
            .all(|&(seq, area)| match area {
                None => false,
                Some(Area::Known(area)) => self.latest[usize::from(area & 1)] > seq,
                Some(Area::Unknown) => self.latest[0] > seq && self.latest[1] > seq,
            })
    }

    fn flying(out: &Out) -> bool {
        out.sends[out.count - 1].1.is_none()
    }

    /// Frees what can be freed at the node's cycle count `cycle`, given which buffers a consumer
    /// is counted on now, and calls `freed` with each.
    pub(crate) fn settle(
        &mut self,
        cycle: u64,
        consumer_on: impl Fn(usize) -> bool,
        mut freed: impl FnMut(usize),
    ) {
        for index in 0..self.slots.len() {
            let Some(mut out) = self.slots[index] else {
                continue;
            };
            if Self::flying(&out) {
                continue;
            }
            let on = consumer_on(index);
            out.taken |= on && out.count == 1;
            if out.replaced.is_none() && self.replaced(&out) {
                out.replaced = Some(cycle);
            }
            let gone = out.taken || out.replaced.is_some_and(|at| cycle >= at + SETTLE_CYCLES);
            if out.back && !on && gone {
                self.slots[index] = None;
                freed(index);
            } else {
                self.slots[index] = Some(out);
            }
        }
    }

    /// The buffer to send again, when every buffer holds a frame and nothing but a consumer's
    /// take can free one: all have gone out and come back, and none is counted on, taken or
    /// replaced. It is the buffer whose newest send is oldest, so the frames come round in order.
    pub(crate) fn stuck(&self, consumer_on: impl Fn(usize) -> bool) -> Option<usize> {
        let mut oldest: Option<(u64, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let out = slot.as_ref()?;
            if Self::flying(out)
                || !out.back
                || out.taken
                || out.replaced.is_some()
                || self.replaced(out)
                || consumer_on(index)
            {
                return None;
            }
            let seq = out.sends[out.count - 1].0;
            if out.count < MAX_SENDS && oldest.is_none_or(|(least, _)| seq < least) {
                oldest = Some((seq, index));
            }
        }
        oldest.map(|(_, index)| index)
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
        holds.settle(cycle, on, |index| freed.push(index));
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
    }

    const STEPS: [Step; 9] = [
        Step::Send(true),
        Step::Send(false),
        Step::Take(0),
        Step::Take(1),
        Step::Give(0),
        Step::Give(1),
        Step::Give(2),
        Step::Idle,
        Step::Idle,
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
        for &step in STEPS.iter().take(8) {
            let mut holds = holds.clone();
            let mut world = world.clone();
            match step {
                Step::Send(known) => {
                    // A cycle in which the node sends: what was sent last cycle comes back.
                    world.cycle += 1;
                    world.counted.clone_from(&world.held);
                    for index in 0..world.returning.len() {
                        if std::mem::take(&mut world.returning[index]) {
                            holds.came_back(index);
                        }
                    }
                    let free = world.free.iter().position(|&f| f);
                    let index = match free {
                        Some(index) => index,
                        None => match holds.stuck(|i| world.counted[i]) {
                            Some(index) => {
                                world.resent += 1;
                                index
                            }
                            None => continue,
                        },
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
                |i| counted[i],
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
        for (buffers, depth) in [(2, 10), (3, 9)] {
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
    fn a_full_ring_nothing_can_free_sends_its_oldest_frame_again() {
        let mut holds = Holds::new(2);
        for index in 0..2 {
            holds.send(index);
            holds.went_out(index, Area::Unknown);
            holds.came_back(index);
        }
        assert!(settled(&mut holds, 5, |_| false).is_empty());
        assert_eq!(holds.stuck(|_| false), Some(0));
        // Not while a consumer is on one: that will free it.
        assert_eq!(holds.stuck(|i| i == 1), None);
        holds.send(0);
        assert_eq!(holds.stuck(|_| false), None, "a send is in flight");
    }
}

//! The reference picture tracker: which decoded frames are references, their picture numbers,
//! the reference picture lists of a slice (8.2.4), and the marking after each reference picture
//! (8.2.5). Frames only.
//!
//! The tracker keeps at most [`DPB_SIZE`] reference frames, each in a slot. A slot number is what
//! the lists hold. [`RefFrame::stamp_ns`] is the stamp of the decode job that produced the
//! frame: a stateless decoder names a reference picture in its controls by that stamp, and the
//! kernel finds the capture buffer that carries it.
//!
//! Output order and the bumping of non-reference pictures are not handled here: a stateless
//! decoder's caller gets each picture when it is decoded.

use crate::bitstream::{Error, Result};
use crate::h264::poc::Poc;
use crate::h264::ps::Sps;
use crate::h264::slice::{MAX_RPL, SliceHeader, SliceKind};

/// The most reference frames the tracker keeps (`max_num_ref_frames` is at most 16).
pub const DPB_SIZE: usize = 16;
/// The entry of a reference picture list that names no reference picture.
pub const NONE: u8 = u8::MAX;

/// A reference frame the tracker keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefFrame {
    /// `frame_num` (0 after a memory management operation 5).
    pub frame_num: u32,
    /// The frame's order counts, as stored.
    pub poc: Poc,
    /// The stamp of the job that decoded the frame (its last slice's, for a frame of several
    /// slices): a whole number of microseconds, in nanoseconds, unique per job.
    pub stamp_ns: u64,
    /// `LongTermFrameIdx`, for a long-term frame.
    pub long_term: Option<u32>,
}

/// The reference picture lists of a slice: per entry, the DPB slot of its reference, or [`NONE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefLists {
    /// The entries of lists 0 and 1; the first `len[list]` of each are active.
    pub entries: [[u8; MAX_RPL]; 2],
    /// `num_ref_idx_lX_active` for lists 0 and 1.
    pub len: [u8; 2],
}

impl RefLists {
    /// The active entries of list `list`.
    pub fn list(&self, list: usize) -> &[u8] {
        &self.entries[list][..usize::from(self.len[list])]
    }
}

/// The reference frames of a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dpb {
    slots: [Option<RefFrame>; DPB_SIZE],
    /// `MaxLongTermFrameIdx`; `None` for "no long-term frame indices".
    max_long_term_idx: Option<u32>,
    /// `PrevRefFrameNum`.
    prev_ref_frame_num: u32,
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

/// `FrameNumWrap` of a frame numbered `frame_num`, seen from the picture numbered `cur`.
fn wrap(frame_num: u32, cur: u32, max: u32) -> i64 {
    let f = i64::from(frame_num);
    if f > i64::from(cur) {
        f - i64::from(max)
    } else {
        f
    }
}

impl Dpb {
    /// An empty tracker.
    pub const fn new() -> Self {
        Self {
            slots: [None; DPB_SIZE],
            max_long_term_idx: None,
            prev_ref_frame_num: 0,
        }
    }

    /// The reference frame in `slot`, if it holds one.
    pub fn get(&self, slot: usize) -> Option<&RefFrame> {
        self.slots.get(slot)?.as_ref()
    }

    /// The reference frames, with their slots.
    pub fn frames(&self) -> impl Iterator<Item = (u8, RefFrame)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|f| (i as u8, f)))
    }

    /// `MaxLongTermFrameIdx`.
    pub const fn max_long_term_idx(&self) -> Option<u32> {
        self.max_long_term_idx
    }

    /// Checks `frame_num` of a non-IDR picture against `PrevRefFrameNum`. A gap (a frame that
    /// is missing from the stream) is refused as unsupported: gap filling is not implemented.
    pub fn check_frame_num(&self, h: &SliceHeader, sps: &Sps) -> Result<()> {
        if h.is_idr() {
            return Ok(());
        }
        let prev = self.prev_ref_frame_num;
        let next = (prev + 1) % sps.max_frame_num();
        if h.frame_num == prev || h.frame_num == next {
            Ok(())
        } else {
            Err(Error::Unsupported("a gap in frame_num"))
        }
    }

    /// The picture number of the frame in `slot` for the picture numbered `cur` (`frame_num`
    /// `cur`, with `max` as `MaxFrameNum`): its `FrameNumWrap` when short-term, or its
    /// `LongTermPicNum` (`LongTermFrameIdx`) when long-term. `None` for an empty slot.
    pub fn pic_num(&self, slot: u8, cur: u32, max: u32) -> Option<i64> {
        let f = self.get(usize::from(slot))?;
        Some(match f.long_term {
            Some(idx) => i64::from(idx),
            None => wrap(f.frame_num, cur, max),
        })
    }

    /// Finds the short-term frame with `PicNum` `pic_num` (its `FrameNumWrap`).
    fn find_short(&self, pic_num: i64, cur: u32, max: u32) -> Option<u8> {
        self.frames()
            .find(|(_, f)| f.long_term.is_none() && wrap(f.frame_num, cur, max) == pic_num)
            .map(|(s, _)| s)
    }

    /// Finds the long-term frame with `LongTermPicNum` (its `LongTermFrameIdx`).
    fn find_long(&self, idx: u32) -> Option<u8> {
        self.frames()
            .find(|(_, f)| f.long_term == Some(idx))
            .map(|(s, _)| s)
    }

    /// The initial lists of a slice (8.2.4.2): short-term frames by picture number (P) or by
    /// order count (B), then long-term frames by `LongTermPicNum`. Returns each list's slots
    /// and its length.
    fn initial(
        &self,
        kind: SliceKind,
        cur: Poc,
        cur_num: u32,
        max: u32,
    ) -> ([u8; DPB_SIZE], usize, [u8; DPB_SIZE], usize) {
        let mut shorts = [NONE; DPB_SIZE];
        let mut ns = 0;
        let mut longs = [NONE; DPB_SIZE];
        let mut nl = 0;
        for (slot, f) in self.frames() {
            if f.long_term.is_some() {
                longs[nl] = slot;
                nl += 1;
            } else {
                shorts[ns] = slot;
                ns += 1;
            }
        }
        let shorts = &mut shorts[..ns];
        let longs = &mut longs[..nl];
        let at = |s: u8| self.slots[usize::from(s)];
        longs.sort_unstable_by_key(|&s| at(s).and_then(|f| f.long_term).unwrap_or(u32::MAX));
        let mut l0 = [NONE; DPB_SIZE];
        let mut l1 = [NONE; DPB_SIZE];
        let (n0, n1);
        if kind == SliceKind::B {
            let poc = cur.frame();
            let mut before: [u8; DPB_SIZE] = [NONE; DPB_SIZE];
            let mut after: [u8; DPB_SIZE] = [NONE; DPB_SIZE];
            let (mut nb, mut na) = (0, 0);
            for &s in shorts.iter() {
                let p = at(s).map_or(0, |f| f.poc.frame());
                if p < poc {
                    before[nb] = s;
                    nb += 1;
                } else {
                    after[na] = s;
                    na += 1;
                }
            }
            before[..nb]
                .sort_unstable_by_key(|&s| std::cmp::Reverse(at(s).map_or(0, |f| f.poc.frame())));
            after[..na].sort_unstable_by_key(|&s| at(s).map_or(0, |f| f.poc.frame()));
            let mut k = 0;
            for &s in before[..nb]
                .iter()
                .chain(after[..na].iter())
                .chain(longs.iter())
            {
                l0[k] = s;
                k += 1;
            }
            n0 = k;
            k = 0;
            for &s in after[..na]
                .iter()
                .chain(before[..nb].iter())
                .chain(longs.iter())
            {
                l1[k] = s;
                k += 1;
            }
            n1 = k;
        } else {
            shorts.sort_unstable_by_key(|&s| {
                std::cmp::Reverse(wrap(at(s).map_or(0, |f| f.frame_num), cur_num, max))
            });
            let mut k = 0;
            for &s in shorts.iter().chain(longs.iter()) {
                l0[k] = s;
                k += 1;
            }
            n0 = k;
            n1 = 0;
        }
        (l0, n0, l1, n1)
    }

    /// The reference picture lists of the slice `h` of a picture with order counts `cur` (8.2.4).
    /// A list longer than its active count is cut to it; a list shorter than it leaves
    /// [`NONE`] entries. An error when a modification names a frame the tracker does not hold.
    pub fn lists(&self, h: &SliceHeader, sps: &Sps, cur: Poc) -> Result<RefLists> {
        let mut out = RefLists {
            entries: [[NONE; MAX_RPL]; 2],
            len: h.num_ref_idx_active,
        };
        if !h.kind.has_list0() {
            return Ok(out);
        }
        let max = sps.max_frame_num();
        let cur_num = h.frame_num;
        let (l0, n0, mut l1, n1) = self.initial(h.kind, cur, cur_num, max);
        // 8.2.4.2.4: a B list 1 identical to list 0, with more than one entry, has its first two
        // entries switched.
        if h.kind == SliceKind::B && n1 > 1 && l0[..n0] == l1[..n1] {
            l1.swap(0, 1);
        }
        for (list, init, n_init) in [(0, l0, n0), (1, l1, n1)] {
            let n = usize::from(h.num_ref_idx_active[list]);
            if n == 0 {
                continue;
            }
            // The active entries, plus one scratch entry for the modification's shift.
            let mut l = [NONE; MAX_RPL + 1];
            let keep = n.min(n_init);
            l[..keep].copy_from_slice(&init[..keep]);
            let mut pred = i64::from(cur_num);
            let mut ref_idx = 0usize;
            for m in h.modifications(list) {
                if ref_idx >= n {
                    return Err(Error::Invalid(
                        "a list modification past the active entries",
                    ));
                }
                let target = match m.idc {
                    0 | 1 => {
                        let abs = i64::from(m.value) + 1;
                        let max = i64::from(max);
                        let mut no_wrap = if m.idc == 0 {
                            let v = pred - abs;
                            if v < 0 { v + max } else { v }
                        } else {
                            let v = pred + abs;
                            if v >= max { v - max } else { v }
                        };
                        pred = no_wrap;
                        if no_wrap > i64::from(cur_num) {
                            no_wrap -= max;
                        }
                        self.find_short(no_wrap, cur_num, max as u32)
                    }
                    _ => self.find_long(m.value),
                }
                .ok_or(Error::Invalid(
                    "a list modification names a missing reference",
                ))?;
                for c in (ref_idx..n).rev() {
                    l[c + 1] = l[c];
                }
                l[ref_idx] = target;
                ref_idx += 1;
                let mut next = ref_idx;
                for c in ref_idx..=n {
                    if l[c] != target {
                        l[next] = l[c];
                        next += 1;
                    }
                }
                for entry in &mut l[next..=n] {
                    *entry = NONE;
                }
            }
            out.entries[list][..n].copy_from_slice(&l[..n]);
        }
        Ok(out)
    }

    /// The marking after the reference picture of header `h`, decoded by the job stamped
    /// `stamp_ns`, with the order counts it keeps (8.2.5). A non-reference picture is not kept.
    /// A memory management operation that names a frame the tracker does not hold does nothing.
    pub fn mark(&mut self, h: &SliceHeader, sps: &Sps, stamp_ns: u64, poc: Poc) {
        if h.nal_ref_idc == 0 {
            return;
        }
        let max = sps.max_frame_num();
        let cur = h.frame_num;
        let mut long = None;
        let mut frame_num = cur;
        if h.is_idr() {
            self.slots = [None; DPB_SIZE];
            if h.long_term_reference {
                long = Some(0);
                self.max_long_term_idx = Some(0);
            } else {
                self.max_long_term_idx = None;
            }
        } else if !h.adaptive_marking {
            let limit = usize::from(sps.max_num_ref_frames).max(1);
            if self.frames().count() >= limit {
                self.evict_short(cur, max);
            }
        } else {
            let cur_num = i64::from(cur);
            for m in h.mmcos() {
                match m.op {
                    1 => {
                        if let Some(s) = self.find_short(cur_num - (i64::from(m.a) + 1), cur, max) {
                            self.slots[usize::from(s)] = None;
                        }
                    }
                    2 => {
                        if let Some(s) = self.find_long(m.a) {
                            self.slots[usize::from(s)] = None;
                        }
                    }
                    3 => {
                        if let Some(s) = self.find_short(cur_num - (i64::from(m.a) + 1), cur, max) {
                            self.free_long_idx(m.b, Some(s));
                            self.slots[usize::from(s)]
                                .as_mut()
                                .expect("found above")
                                .long_term = Some(m.b);
                        }
                    }
                    4 => {
                        self.max_long_term_idx = m.b.checked_sub(1);
                        for slot in &mut self.slots {
                            if let Some(f) = slot
                                && let Some(idx) = f.long_term
                                && self.max_long_term_idx.is_none_or(|max_idx| idx > max_idx)
                            {
                                *slot = None;
                            }
                        }
                    }
                    5 => {
                        self.slots = [None; DPB_SIZE];
                        self.max_long_term_idx = None;
                        frame_num = 0;
                    }
                    6 => {
                        self.free_long_idx(m.b, None);
                        long = Some(m.b);
                    }
                    _ => {}
                }
            }
        }
        let frame = RefFrame {
            frame_num,
            poc,
            stamp_ns,
            long_term: long,
        };
        let slot = match self.slots.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                self.evict_short(cur, max);
                self.slots.iter().position(Option::is_none).unwrap_or(0)
            }
        };
        self.slots[slot] = Some(frame);
        self.prev_ref_frame_num = frame_num;
    }

    /// Removes the short-term frame with the smallest `FrameNumWrap`, the sliding window's
    /// choice. Does nothing when there is none.
    fn evict_short(&mut self, cur: u32, max: u32) {
        let victim = self
            .frames()
            .filter(|(_, f)| f.long_term.is_none())
            .min_by_key(|(_, f)| wrap(f.frame_num, cur, max))
            .map(|(s, _)| s);
        if let Some(s) = victim {
            self.slots[usize::from(s)] = None;
        }
    }

    /// Unmarks the long-term frames with `LongTermFrameIdx` `idx`, but not the frame in `keep`.
    fn free_long_idx(&mut self, idx: u32, keep: Option<u8>) {
        for (s, slot) in self.slots.iter_mut().enumerate() {
            if keep == Some(s as u8) {
                continue;
            }
            if slot.is_some_and(|f| f.long_term == Some(idx)) {
                *slot = None;
            }
        }
    }
}

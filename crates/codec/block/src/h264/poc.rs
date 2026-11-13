//! The picture order count of a frame (8.2.1), for `pic_order_cnt_type` 0, 1 and 2, and the
//! state that carries from one picture to the next.
//!
//! Frames only: a frame's top and bottom field order counts are both set, and the frame's
//! count is the smaller of the two. A memory management operation 5 is applied after its
//! picture is decoded: [`Pic::stored`] gives the picture's counts as the DPB keeps them.

use crate::bitstream::{Error, Result};
use crate::h264::ps::Sps;
use crate::h264::slice::SliceHeader;

/// A frame's order counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Poc {
    /// `TopFieldOrderCnt`.
    pub top: i32,
    /// `BottomFieldOrderCnt`.
    pub bottom: i32,
}

impl Poc {
    /// `PicOrderCnt` of the frame: the smaller of its two counts.
    pub const fn frame(self) -> i32 {
        if self.top < self.bottom {
            self.top
        } else {
            self.bottom
        }
    }
}

/// A picture's counts as it is decoded, with the state its successor needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pic {
    /// The counts for decoding the picture.
    pub poc: Poc,
    /// `FrameNumOffset` (type 1 and 2).
    pub frame_num_offset: i64,
    /// `PicOrderCntMsb` (type 0).
    pub msb: i32,
    /// Whether the picture has a memory management operation 5.
    pub mmco5: bool,
}

impl Pic {
    /// The counts the picture keeps after its memory management operation 5 (8.2.1): both
    /// reduced by the smaller one. The picture's own decode used [`Pic::poc`].
    pub fn stored(&self) -> Poc {
        if !self.mmco5 {
            return self.poc;
        }
        let temp = self.poc.frame();
        Poc {
            top: self.poc.top - temp,
            bottom: self.poc.bottom - temp,
        }
    }
}

/// The state the order counts of the next picture depend on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PocState {
    /// `prevPicOrderCntMsb` and `prevPicOrderCntLsb` (type 0): of the previous reference picture.
    prev_msb: i32,
    prev_lsb: i32,
    /// `prevFrameNumOffset` and `prevFrameNum` (types 1 and 2): of the previous picture.
    prev_frame_num_offset: i64,
    prev_frame_num: u32,
}

impl PocState {
    /// The counts of the picture of `h`, in `sps`.
    pub fn compute(&self, h: &SliceHeader, sps: &Sps) -> Result<Pic> {
        let mmco5 = h.mmcos().iter().any(|m| m.op == 5);
        let max_frame_num = i64::from(sps.max_frame_num());
        let frame_num = i64::from(h.frame_num);
        let frame_num_offset = if h.is_idr() {
            0
        } else {
            let prev_offset = self.prev_frame_num_offset;
            if i64::from(self.prev_frame_num) > frame_num {
                prev_offset + max_frame_num
            } else {
                prev_offset
            }
        };
        let (poc, msb) = match sps.poc_type {
            0 => {
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let (prev_msb, prev_lsb) = if h.is_idr() {
                    (0, 0)
                } else {
                    (self.prev_msb, self.prev_lsb)
                };
                let lsb = h.poc_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                (
                    Poc {
                        top,
                        bottom: top + h.delta_poc_bottom,
                    },
                    msb,
                )
            }
            1 => {
                let n = sps.offset_for_ref_frame.len();
                let mut abs = if n != 0 {
                    frame_num_offset + frame_num
                } else {
                    0
                };
                if h.nal_ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i64;
                if abs > 0 {
                    let n = n as i64;
                    let cycle = (abs - 1) / n;
                    let in_cycle = ((abs - 1) % n) as usize;
                    let per_cycle: i64 =
                        sps.offset_for_ref_frame.iter().map(|&o| i64::from(o)).sum();
                    expected = cycle * per_cycle;
                    for offset in &sps.offset_for_ref_frame[..=in_cycle] {
                        expected += i64::from(*offset);
                    }
                }
                if h.nal_ref_idc == 0 {
                    expected += i64::from(sps.offset_for_non_ref_pic);
                }
                let top = expected + i64::from(h.delta_poc[0]);
                let bottom =
                    top + i64::from(sps.offset_for_top_to_bottom_field) + i64::from(h.delta_poc[1]);
                let poc = Poc {
                    top: i32::try_from(top).map_err(|_| Error::Invalid("a picture order count"))?,
                    bottom: i32::try_from(bottom)
                        .map_err(|_| Error::Invalid("a picture order count"))?,
                };
                (poc, 0)
            }
            _ => {
                let temp = if h.is_idr() {
                    0
                } else if h.nal_ref_idc == 0 {
                    2 * (frame_num_offset + frame_num) - 1
                } else {
                    2 * (frame_num_offset + frame_num)
                };
                let temp =
                    i32::try_from(temp).map_err(|_| Error::Invalid("a picture order count"))?;
                (
                    Poc {
                        top: temp,
                        bottom: temp,
                    },
                    0,
                )
            }
        };
        Ok(Pic {
            poc,
            frame_num_offset,
            msb,
            mmco5,
        })
    }

    /// Records a decoded picture so the next one's counts follow from it.
    pub fn commit(&mut self, h: &SliceHeader, pic: &Pic) {
        if h.nal_ref_idc != 0 {
            if pic.mmco5 {
                self.prev_msb = 0;
                self.prev_lsb = pic.stored().top;
            } else {
                self.prev_msb = pic.msb;
                self.prev_lsb = h.poc_lsb as i32;
            }
        }
        if pic.mmco5 {
            self.prev_frame_num_offset = 0;
            self.prev_frame_num = 0;
        } else {
            self.prev_frame_num_offset = pic.frame_num_offset;
            self.prev_frame_num = h.frame_num;
        }
    }
}

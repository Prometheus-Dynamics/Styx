//! The parameter sets of Styx's H.264 streams, for an encoder that writes only the parameter sets
//! itself: a hardware block that codes the slices.
//!
//! [`ParameterSets::write`] gives the same SPS and PPS as [`crate::StyxH264`] writes for an IDR, as
//! Annex B NAL units with four-byte start codes. The stream's shape is fixed and stated by
//! [`Shape`], so a block that writes its own slice headers is set to the same values.

use styx_codec_api::CodecError;

use crate::bits::{Bits, nal};
use crate::headers;

/// The choices every Styx H.264 stream makes, in its SPS and PPS. A slice written by anyone else
/// must agree with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// `profile_idc` (66, Baseline) with `constraint_set0` and `constraint_set1`: Constrained
    /// Baseline.
    pub profile_idc: u8,
    /// `log2_max_frame_num_minus4 + 4`.
    pub log2_max_frame_num: u32,
    /// `pic_order_cnt_type`: 2, the order is the decoding order, and slices carry no POC.
    pub pic_order_cnt_type: u32,
    /// `entropy_coding_mode_flag`: false, CAVLC.
    pub cabac: bool,
    /// `pic_init_qp_minus26 + 26`.
    pub pic_init_qp: u8,
    /// `chroma_qp_index_offset`.
    pub chroma_qp_index_offset: i8,
    /// `deblocking_filter_control_present_flag`: slices say whether the filter runs.
    pub deblocking_filter_control_present: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `num_ref_idx_l0_default_active_minus1 + 1`.
    pub num_ref_idx_l0_default: u32,
    /// `seq_parameter_set_id` and `pic_parameter_set_id`.
    pub ids: u32,
}

/// The shape [`ParameterSets::write`] writes.
pub const SHAPE: Shape = Shape {
    profile_idc: 66,
    log2_max_frame_num: headers::LOG2_MAX_FRAME_NUM,
    pic_order_cnt_type: 2,
    cabac: false,
    pic_init_qp: 26,
    chroma_qp_index_offset: 0,
    deblocking_filter_control_present: true,
    constrained_intra_pred: false,
    num_ref_idx_l0_default: 1,
    ids: 0,
};

/// The SPS and PPS of a picture size, written into buffers kept from call to call.
#[derive(Debug, Default)]
pub struct ParameterSets {
    bits: Bits,
    out: Vec<u8>,
}

impl ParameterSets {
    pub fn new() -> Self {
        Self::default()
    }

    /// The SPS, then the PPS, for a `width` x `height` picture, each a NAL unit with a four-byte
    /// start code. After the first call for a size, a call allocates nothing.
    ///
    /// # Errors
    ///
    /// [`CodecError::Rejected`] for an odd or zero size (4:2:0 pictures are cropped in pairs).
    pub fn write(&mut self, width: usize, height: usize) -> Result<&[u8], CodecError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(CodecError::Rejected(format!(
                "styx-h264: a {width}x{height} picture is not an even size"
            )));
        }
        let mbs = (width.div_ceil(16), height.div_ceil(16));
        self.out.clear();
        self.bits.clear();
        headers::sps(&mut self.bits, (width, height), mbs);
        nal(&mut self.out, 3, 7, self.bits.bytes());
        self.bits.clear();
        headers::pps(&mut self.bits);
        nal(&mut self.out, 3, 8, self.bits.bytes());
        Ok(&self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sets_are_two_nal_units_and_the_same_as_the_encoders() {
        let mut sets = ParameterSets::new();
        let bytes = sets.write(320, 240).unwrap().to_vec();
        assert_eq!(&bytes[..5], &[0, 0, 0, 1, 0x67]);
        let pps_at = bytes
            .windows(5)
            .position(|w| w == [0, 0, 0, 1, 0x68])
            .expect("a PPS");
        assert!(pps_at > 5);
        // The software encoder's first frame starts with the same two NAL units.
        let mut out = Vec::new();
        let mut enc = crate::StyxH264::new(26, 30);
        let y = vec![128u8; 320 * 240];
        let c = vec![128u8; 160 * 120];
        enc.encode(
            &crate::StyxH264Input::I420 {
                y: &y,
                u: &c,
                v: &c,
                width: 320,
                height: 240,
                y_stride: 320,
                c_stride: 160,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(&out[..bytes.len()], &bytes[..]);
        assert!(sets.write(321, 240).is_err());
    }
}

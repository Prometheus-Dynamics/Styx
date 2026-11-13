//! The block's controls for a slice, and the layout that writes them.
//!
//! The controls of a stateless H.264 decode are the kernel's V4L2 H.264 structures
//! (`V4L2_CID_STATELESS_H264_*`). Lemnos owns those structures, and its lock 04d09360 does not
//! have them, so Styx does not define them: a [`Layout`] is supplied by the caller, and writes
//! each control's bytes from the parsed [`SliceView`]. Styx's own parsing, reference tracking and
//! job work do not depend on the layout.

use styx_codec_api::CodecError;

use crate::h264::dpb::{Dpb, RefLists};
use crate::h264::poc::Poc;
use crate::h264::ps::{Pps, Sps};
use crate::h264::slice::SliceHeader;

/// The controls a slice's job carries, as (control id, bytes) in the order the block gets them.
/// Its buffers are reused from slice to slice.
#[derive(Clone, Debug, Default)]
pub struct Controls {
    bytes: Vec<u8>,
    items: Vec<(u32, usize, usize)>,
}

impl Controls {
    /// Empties the controls, keeping their buffers.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.items.clear();
    }

    /// Adds a control with `bytes`.
    pub fn push(&mut self, tag: u32, bytes: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        self.items.push((tag, start, self.bytes.len()));
    }

    /// The controls, in order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        self.items
            .iter()
            .map(|&(tag, start, end)| (tag, &self.bytes[start..end]))
    }

    /// The number of controls.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no controls.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// What a layout is given for one slice: its parameter sets and header, the picture's order
/// counts, the reference picture lists (DPB slots), and the DPB they name.
#[derive(Clone, Copy, Debug)]
pub struct SliceView<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub header: &'a SliceHeader,
    /// The picture's order counts, before any memory management operation 5.
    pub poc: Poc,
    pub lists: &'a RefLists,
    pub dpb: &'a Dpb,
}

/// Writes the block's controls for a slice. An error refuses the frame: the layout returns
/// [`CodecError::Rejected`] for a slice it cannot express.
pub trait Layout: Send {
    /// Writes the controls of `slice` into `out`, which the decoder has cleared.
    fn write(&mut self, slice: &SliceView<'_>, out: &mut Controls) -> Result<(), CodecError>;
}

//! The H.264 decode job's work, built as bytes for `codec.block/1`: the same items as the HEVC
//! job ([`crate::work`]), and one reference per DPB slot that holds a frame.
//!
//! Reference slots: slot [`REFERENCE_SLOT`] + `n` names DPB slot `n`, whose buffer is the
//! reference frame's picture. The lists the controls name are DPB slots, so the block maps a
//! list entry to the reference slot it was given.

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Builder;

use crate::h264::dpb::DPB_SIZE;
pub use crate::work::{
    DoesNotFit, FIRST_CONTROL, SETUP_BYTES, SLOT_PICTURE, SLOT_SOURCE, Setup, TAG_SETUP, TAG_STAMP,
};

/// The first reference slot: DPB slot `n` is reference slot `REFERENCE_SLOT + n`.
pub const REFERENCE_SLOT: u32 = 2;
/// Bytes of work one job is given room for. An H.264 slice's controls take more than HEVC's.
pub const WORK_BYTES: usize = 16 * 1024;

/// One H.264 decode job's inputs.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub setup: Setup,
    pub stamp_ns: u64,
    /// The stream buffer holding the slice, from its first byte.
    pub source: u32,
    /// Bytes of the source the block reads: the slice with its start code.
    pub source_len: u32,
    /// The buffer the picture is decoded into.
    pub picture: u32,
    /// The reference frames: (DPB slot, its picture buffer), at most [`DPB_SIZE`].
    pub references: &'a [(u8, u32)],
    /// Control id and its bytes, in the order the block gets them.
    pub controls: &'a [(u32, &'a [u8])],
}

/// Writes `job`'s work into `out` and returns its length in bytes.
pub fn build(out: &mut [u8], job: &Job<'_>) -> Result<usize, DoesNotFit> {
    if job.references.len() > DPB_SIZE {
        return Err(DoesNotFit);
    }
    let mut work = Builder::new(out).ok_or(DoesNotFit)?;
    let source = BufferRef {
        slot: SLOT_SOURCE,
        buffer: job.source,
        offset: 0,
        length: job.source_len,
    };
    let picture = BufferRef {
        slot: SLOT_PICTURE,
        buffer: job.picture,
        offset: 0,
        length: 0,
    };
    work.reference(source).ok_or(DoesNotFit)?;
    work.reference(picture).ok_or(DoesNotFit)?;
    for &(dpb_slot, buffer) in job.references {
        let reference = BufferRef {
            slot: REFERENCE_SLOT + u32::from(dpb_slot),
            buffer,
            offset: 0,
            length: 0,
        };
        work.reference(reference).ok_or(DoesNotFit)?;
    }
    work.item(TAG_SETUP, &job.setup.to_bytes())
        .ok_or(DoesNotFit)?;
    work.item(TAG_STAMP, &job.stamp_ns.to_le_bytes())
        .ok_or(DoesNotFit)?;
    for (tag, bytes) in job.controls {
        if *tag < FIRST_CONTROL {
            return Err(DoesNotFit);
        }
        work.item(*tag, bytes).ok_or(DoesNotFit)?;
    }
    Ok(work.finish())
}

//! The H.264 decode job's work, built as bytes for `codec.block/1`: the same shape as the HEVC job
//! ([`crate::work`]). Its references are the bitstream (slot [`SLOT_SOURCE`]) and the picture
//! (slot [`SLOT_PICTURE`]) and nothing else. The reference pictures of a slice are not slots:
//! they are named in the controls (`DpbEntry::reference_ts`), by the stamp of the job that
//! decoded each, and the block finds them among its capture buffers.

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Builder;

pub use crate::work::{
    DoesNotFit, FIRST_CONTROL, SETUP_BYTES, SLOT_PICTURE, SLOT_SOURCE, Setup, TAG_SETUP, TAG_STAMP,
};

/// Bytes of work one job is given room for. An H.264 slice's controls take about 3 KiB, the
/// SPS and the decode parameters being the largest; this leaves room for a full set.
pub const WORK_BYTES: usize = 8 * 1024;

/// One H.264 decode job's inputs: one slice, its picture, and the controls that describe it.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub setup: Setup,
    /// The stamp of this job, which the picture carries if it is the last slice's job.
    pub stamp_ns: u64,
    /// The stream buffer holding the slice, from its first byte.
    pub source: u32,
    /// Bytes of the source the block reads: the slice with its start code.
    pub source_len: u32,
    /// The buffer the picture is decoded into.
    pub picture: u32,
    /// Control id and its bytes, in the order the block gets them.
    pub controls: &'a [(u32, &'a [u8])],
}

/// Writes `job`'s work into `out` and returns its length in bytes.
pub fn build(out: &mut [u8], job: &Job<'_>) -> Result<usize, DoesNotFit> {
    let mut work = Builder::new(out).ok_or(DoesNotFit)?;
    let refs = [
        BufferRef {
            slot: SLOT_SOURCE,
            buffer: job.source,
            offset: 0,
            length: job.source_len,
        },
        BufferRef {
            slot: SLOT_PICTURE,
            buffer: job.picture,
            offset: 0,
            length: 0,
        },
    ];
    for reference in refs {
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

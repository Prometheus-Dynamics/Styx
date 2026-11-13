//! The `v4l2-stateless` block's work for one decode job, built as bytes for `codec.block/1`.
//!
//! Lemnos's block (`drivers/m2m-decode`, `StatelessBlock`, lock 04d09360) takes the job's work
//! in the layout of `lemnos_contract_codec_block::work`, with these items and references:
//!
//! - Item [`TAG_SETUP`]: five `u32`s, the coded format (`S265`), the picture's width and height,
//!   the picture format (`NV12`), and its memory plane count.
//! - Item [`TAG_STAMP`]: a `u64` of nanoseconds the job is known by: a whole number of
//!   microseconds, unique per job (see [`Stamps`]).
//! - Items whose tag is a control id ([`FIRST_CONTROL`] and up): the control's bytes, as the
//!   kernel's `struct` layout (see [`crate::hevc::controls`]).
//! - Reference [`SLOT_SOURCE`]: the bitstream, from its first byte at the start of a stream
//!   buffer (`offset` 0, `length` the bytes of the slice with its start code).
//! - Reference [`SLOT_PICTURE`]: the buffer the picture is decoded into (one memory plane).
//!
//! A reference picture is not a slot. A decoder names it inside its controls, by the stamp of
//! the job that decoded it (H.264's `reference_ts`), and the block finds that capture buffer
//! itself. So the only references a job carries are the two above.
//!
//! Styx decides what the work says; the block checks it and runs it. Nothing here touches a
//! device.

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Builder;

/// The block's name, as the block gives it.
pub const BLOCK_NAME: &str = "v4l2-stateless";
/// The item that says what the job's session is set up for.
pub const TAG_SETUP: u32 = 1;
/// The item that says what the job's picture is known by.
pub const TAG_STAMP: u32 = 2;
/// The lowest tag that is a control id (`V4L2_CTRL_CLASS_USER`).
pub const FIRST_CONTROL: u32 = 0x0098_0000;
/// The stamps of one decoder's jobs. A stamp is a whole number of microseconds, in nanoseconds
/// (`v4l2_timeval_to_ns`), and no two jobs of a decoder share one: the block refuses a finer
/// stamp, and a reference names its picture by the stamp of the job that decoded it. So a
/// decoder numbers its jobs itself, rather than taking the input frame's timestamp.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stamps {
    last_us: u64,
}

impl Stamps {
    /// The next stamp: the microseconds after the last, in nanoseconds. `None` only when the
    /// count runs out, which a decoder reports as a refused frame.
    pub fn next(&mut self) -> Option<u64> {
        let us = self.last_us.checked_add(1)?;
        let ns = us.checked_mul(1_000)?;
        self.last_us = us;
        Some(ns)
    }
}

/// Bytes of a [`TAG_SETUP`] item.
pub const SETUP_BYTES: usize = 5 * 4;
/// The reference slot of the bitstream.
pub const SLOT_SOURCE: u32 = 0;
/// The reference slot of the picture's first memory plane.
pub const SLOT_PICTURE: u32 = 1;
/// Bytes of work one job is given room for. Well above what an IDR slice's controls take.
pub const WORK_BYTES: usize = 4096;

/// What a job's [`TAG_SETUP`] item says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// The coded format, as its V4L2 fourcc (`PIX_FMT_HEVC_SLICE`).
    pub coded: u32,
    pub width: u32,
    pub height: u32,
    /// The picture format, as its V4L2 fourcc (`NV12`).
    pub picture: u32,
    /// Memory planes of the picture format.
    pub planes: u32,
}

impl Setup {
    /// The item's bytes, little-endian.
    pub fn to_bytes(self) -> [u8; SETUP_BYTES] {
        let mut out = [0; SETUP_BYTES];
        let words = [
            self.coded,
            self.width,
            self.height,
            self.picture,
            self.planes,
        ];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *chunk = word.to_le_bytes();
        }
        out
    }
}

/// One decode job's inputs: the setup, the picture's stamp, the stream buffers the bitstream
/// and the picture are in, and the controls.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub setup: Setup,
    pub stamp_ns: u64,
    /// The stream buffer holding the slice, from its first byte.
    pub source: u32,
    /// Bytes of the source the block reads: the slice with its start code.
    pub source_len: u32,
    /// The stream buffer the picture is decoded into.
    pub picture: u32,
    /// Control id and its bytes, in the order the block gets them.
    pub controls: &'a [(u32, &'a [u8])],
}

/// Why a job's work could not be written: it does not fit `out`, or an item is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoesNotFit;

/// Writes `job`'s work into `out` and returns its length in bytes. Every reference comes before
/// the items, as the work layout requires.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_whole_microseconds_and_never_repeat() {
        let mut stamps = Stamps::default();
        let first: Vec<u64> = (0..1000).map(|_| stamps.next().unwrap()).collect();
        assert_eq!(first[0], 1_000);
        assert!(first.iter().all(|ns| ns % 1_000 == 0));
        assert!(first.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_stamp_sequence_that_runs_out_says_so() {
        let mut stamps = Stamps {
            last_us: u64::MAX / 1_000,
        };
        assert_eq!(stamps.next(), None);
    }
}

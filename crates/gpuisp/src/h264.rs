//! An H.264 encoder's motion search on the GPU: exhaustive whole-pel search per macroblock
//! (`shaders/h264_me.comp`), the result of `styx_core::simd::h264::full_search` for every
//! macroblock of a picture, bit for bit. The encoder refines from these (Lagrangian costs and
//! the macroblock decisions are serial and stay on the CPU).

use styx_core::simd::h264::SEARCH_RANGE_MAX;

use crate::convert::{ARG_WORDS, GpuConvert, check_rows};
use crate::error::GpuError;

/// SPIR-V of `shaders/h264_me.comp` (`shaders/build.sh` makes it).
pub(crate) const SPIRV: &[u8] = include_bytes!("../shaders/h264_me.spv");

/// One macroblock's best whole-pel offset and its SAD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MotionCandidate {
    pub mv: [i32; 2],
    pub sad: u32,
}

impl GpuConvert {
    /// Motion search of every 16x16 macroblock of `cur` in `reference` (luma, `w` x `h` with
    /// rows `w` apart, both multiples of 16), offsets up to `range` (at most 16) either way:
    /// `out` (resized to the macroblock count, row-major) gets what `full_search` gives.
    pub fn motion_search(
        &mut self,
        cur: &[u8],
        reference: &[u8],
        (w, h): (usize, usize),
        range: i32,
        out: &mut Vec<MotionCandidate>,
    ) -> Result<(), GpuError> {
        assert!(
            w > 0 && h > 0 && w.is_multiple_of(16) && h.is_multiple_of(16),
            "{w}x{h}"
        );
        assert!((0..=SEARCH_RANGE_MAX).contains(&range), "range {range}");
        check_rows("current", cur.len(), w, w, h);
        check_rows("reference", reference.len(), w, w, h);
        let (mb_w, mb_h) = (w / 16, h / 16);
        let plane = w * h;
        // The reference at a word boundary.
        let ref_off = plane.next_multiple_of(4);
        self.grow(ref_off + plane, mb_w * mb_h * 8)?;
        let input = self.input.as_mut().expect("grown");
        input.write(0, &cur[..plane]);
        input.write(ref_off, &reference[..plane]);
        input.flush()?;
        let mut words = [0u32; ARG_WORDS];
        words[..6].copy_from_slice(&[
            mb_w as u32,
            mb_h as u32,
            w as u32,
            h as u32,
            ref_off as u32,
            range as u32,
        ]);
        let mut bytes = [0u8; ARG_WORDS * 4];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        self.dispatch(self.h264_me, &bytes, (mb_w as u32, mb_h as u32))?;
        let output = self.output.as_ref().expect("grown");
        output.invalidate()?;
        let d = 2 * range + 1;
        let found = output.bytes()[..mb_w * mb_h * 8].as_chunks::<8>().0;
        out.clear();
        out.extend(found.iter().map(|p| {
            let key = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            let sad = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
            let c = (key & 2047) as i32;
            MotionCandidate {
                mv: [c % d - range, c / d - range],
                sad,
            }
        }));
        Ok(())
    }
}

//! A: `styx-core` alone. A receiver's DMA fills one of three buffers; each filled buffer is
//! lent in place to a `FrameLease` (a `MemoryRegion` whose release hands the buffer back) and
//! sent through a bounded queue that keeps the newest frame; the consumer reads it.

use alloc::boxed::Box;
use alloc::vec::Vec;

use styx_core::buffer::{FrameLease, FrameMeta, MemoryRegion, PlaneLayout, RegionHooks};
use styx_core::format::{ColorSpace, FourCc, MediaFormat, Resolution};
use styx_core::queue::{QueueOverflow, RecvOutcome, bounded_with};
use styx_core::sync::{AtomicU32, Ordering};

use crate::Report;
use crate::board::{expose, frame_buffer};

/// Receiver buffers.
const BUFFERS: u32 = 3;

/// Buffers the receiver may fill (a bit each).
static FREE: AtomicU32 = AtomicU32::new(0);

/// A buffer lent to frames: given back to the receiver when the last view drops.
struct Slot(u32);

impl RegionHooks for Slot {
    fn release(&self) {
        FREE.fetch_or(1 << self.0, Ordering::Release);
    }
}

/// `frames` frames of `width` x `height` RAW10 (16-bit samples) from receiver to consumer.
pub fn run(width: u32, height: u32, frames: u32) -> Report {
    let stride = width as usize * 2;
    let len = stride * height as usize;
    let mut buffers: Vec<Box<[u8]>> = (0..BUFFERS).map(|_| frame_buffer(len)).collect();
    FREE.store((1 << BUFFERS) - 1, Ordering::Release);
    let mut report = Report::new();
    let Some(resolution) = Resolution::new(width, height) else {
        return report;
    };
    let format = MediaFormat::new(FourCc::new(*b"BG16"), resolution, ColorSpace::Unknown);
    let (tx, rx) = bounded_with::<FrameLease>(2, QueueOverflow::DropOldest);
    for seq in 0..frames {
        // The DMA-complete interrupt: the oldest free buffer was filled.
        let free = FREE.load(Ordering::Acquire);
        if free != 0 {
            let slot = free.trailing_zeros();
            FREE.fetch_and(!(1 << slot), Ordering::AcqRel);
            let buffer = &mut buffers[slot as usize];
            expose(buffer, width as usize, 10_000 + seq * 100, 256);
            // SAFETY: the buffer outlives the frames (they are all dropped before `buffers`)
            // and is not written again until its frame's release puts it back in `FREE`.
            let region = unsafe { MemoryRegion::from_raw(buffer.as_ptr(), len, Slot(slot)) };
            let meta = FrameMeta::new(format, u64::from(seq) * 33_333_333);
            let layout = PlaneLayout {
                offset: 0,
                len,
                stride,
            };
            let _ = tx.send(FrameLease::from_region(
                meta,
                smallvec::smallvec![layout],
                region,
            ));
        }
        // The consumer.
        if let RecvOutcome::Data(frame) = rx.recv() {
            let planes = frame.planes();
            report.frames += 1;
            report.mix(&planes[0].data()[..64]);
        }
    }
    drop((tx, rx));
    drop(buffers);
    report
}

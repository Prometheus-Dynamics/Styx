//! Styx's footprint on a microcontroller, measured on firmware images (docs/mcu.md).
//!
//! Four configurations, each a firmware binary (`src/bin`) over the same mock board
//! ([`board`]: the part a real port replaces), each a superset of the one before:
//!
//! * [`a`]: `styx-core` alone: frames a receiver's DMA filled, wrapped in place as
//!   `FrameLease`s, handed through a bounded queue to a consumer.
//! * [`b`]: A + the sensor driver (compiled description, frame-exact control schedule, embedded
//!   data) + `styx-runtime`'s `Camera`: raw frames with the values that produced them, a
//!   manual exposure ramp requested frame-exactly.
//! * [`c`]: B + AE and AWB on statistics taken from the raw frames, without an ISP (a mono or
//!   YUV sensor whose own ISP needs only exposure control).
//! * [`d`]: C + the software ISP (Bayer to NV12) with the full 3A (AE, AWB, ALSC, CCM,
//!   contrast), a still bracket reprocessed at full quality, the metrics counters.
//!
//! The binaries are built for `thumbv7em-none-eabihf` (Cortex-M4F/M7) and `thumbv6m-none-eabi`
//! (Cortex-M0+) with the `mcu` profile (`opt-level = "z"`, fat LTO, one codegen unit, abort on
//! panic); `scripts/mcu-size.sh` measures them. On the host the same `no_std` code runs under a
//! counting allocator (`tests/heap.rs`), which gives each configuration's heap high-water mark
//! with the frame buffers ([`frame_memory`]) counted apart.

#![no_std]

extern crate alloc;

pub mod a;
pub mod b;
pub mod board;
pub mod c;
pub mod d;
#[cfg(all(target_arch = "arm", target_os = "none"))]
pub mod rt;

use portable_atomic::{AtomicBool, Ordering};

/// Set while frame buffers are allocated ([`frame_memory`]): a measuring allocator counts what
/// is allocated meanwhile as frame memory.
pub static FRAME_MEMORY: AtomicBool = AtomicBool::new(false);

/// Runs `alloc` (the allocation of a frame buffer: a receiver's buffer, an ISP output) with
/// [`FRAME_MEMORY`] set.
pub fn frame_memory<T>(alloc: impl FnOnce() -> T) -> T {
    FRAME_MEMORY.store(true, Ordering::Relaxed);
    let v = alloc();
    FRAME_MEMORY.store(false, Ordering::Relaxed);
    v
}

/// What a run did, for the host tests (the firmware keeps only the fingerprint, so nothing it
/// computed is dead code).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// FNV-1a over what the consumer read and the loop produced.
    pub fingerprint: u64,
    /// Raw frames the consumer got.
    pub frames: u32,
    /// Of them, frames whose exposure was the one requested for that frame (B: the manual
    /// ramp, landing frame-exactly).
    pub exact: u32,
    /// The first frame AE reported a lock after (C, D).
    pub ae_locked_at: Option<u64>,
    /// Still shots taken (D with the bracket).
    pub shots: u64,
}

impl Report {
    fn new() -> Self {
        Self {
            fingerprint: FNV,
            ..Self::default()
        }
    }

    fn mix(&mut self, bytes: &[u8]) {
        self.fingerprint = fingerprint(self.fingerprint, bytes);
    }

    fn ae(&mut self, seq: u64, locked: bool) {
        if locked && self.ae_locked_at.is_none() {
            self.ae_locked_at = Some(seq);
        }
        self.mix(&[u8::from(locked)]);
    }
}

/// QQVGA: the size the firmware images run at.
pub const QQVGA: (u32, u32) = (160, 120);
/// QVGA.
pub const QVGA: (u32, u32) = (320, 240);

/// FNV-1a over `bytes`: a cheap fingerprint, so nothing the configurations compute is dead.
pub fn fingerprint(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The FNV-1a offset basis.
pub const FNV: u64 = 0xcbf2_9ce4_8422_2325;

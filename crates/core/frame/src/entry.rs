//! One frame in a buffer.

use core::num::NonZeroU32;

use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};

use crate::layout::Layouts;

/// One frame: which it is, when, its format and where its planes are in the buffer.
///
/// Plain data, `Copy`, written into the buffer's slot when the frame is taken and read in place
/// by every share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameEntry {
    /// The frame's identity, as the producer numbers it (for the native path, Lemnos's opaque
    /// frame id). Styx gives it no meaning beyond equality and order.
    pub id: u64,
    /// When the frame was captured: `CLOCK_MONOTONIC`, in nanoseconds. Every backend converts to
    /// this clock; sources without a clock stamp arrival on it.
    pub timestamp_ns: u64,
    pub format: MediaFormat,
    pub layouts: Layouts,
}

impl FrameEntry {
    pub const fn new(id: u64, timestamp_ns: u64, format: MediaFormat, layouts: Layouts) -> Self {
        Self {
            id,
            timestamp_ns,
            format,
            layouts,
        }
    }

    /// What a buffer's slot holds before its first frame.
    pub(crate) const EMPTY: Self = Self {
        id: 0,
        timestamp_ns: 0,
        format: MediaFormat::new(
            FourCc::new([0; 4]),
            Resolution {
                width: NonZeroU32::MIN,
                height: NonZeroU32::MIN,
            },
            ColorSpace::Unknown,
            ColorRange::Unknown,
        ),
        layouts: Layouts::EMPTY,
    };
}

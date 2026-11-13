//! One frame's statistics buffer, read by the library's region numbers.
//!
//! The library gives each part's place and size and nothing about the bytes inside: the field
//! encodings of auto exposure, auto white balance, auto focus, the histograms and the rest are not
//! in it, so this module does not decode them (see [`crate::gaps`]).

use crate::layout::statistics_region;
use crate::library::layout::STATISTICS_BYTES;

/// A statistics buffer of at least [`STATISTICS_BYTES`] bytes.
#[derive(Debug, Clone, Copy)]
pub struct StatisticsView<'a> {
    bytes: &'a [u8],
}

impl<'a> StatisticsView<'a> {
    /// `bytes` as a statistics buffer, or `None` when it is shorter than one.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        (bytes.len() >= STATISTICS_BYTES).then_some(Self { bytes })
    }

    /// The bytes of statistics region `region`, or `None` past the last one.
    #[must_use]
    pub fn region(&self, region: u8) -> Option<&'a [u8]> {
        let r = statistics_region(region)?;
        self.bytes
            .get(r.offset as usize..(r.offset + r.bytes) as usize)
    }
}

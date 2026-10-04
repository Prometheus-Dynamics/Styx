//! Tiling types (libpisp `tiling/types.hpp`, `pisp_tiling.hpp`; BSD-2-Clause, Copyright (C)
//! 2021 - 2023, Raspberry Pi Ltd).

use alloc::string::String;
use core::fmt;

/// Direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Horizontal.
    X,
    /// Vertical.
    Y,
}

/// A size or pair of values per direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Length2 {
    /// Horizontal.
    pub dx: i32,
    /// Vertical.
    pub dy: i32,
}

impl Length2 {
    /// A pair.
    pub const fn new(dx: i32, dy: i32) -> Self {
        Self { dx, dy }
    }
    pub(super) fn get(self, d: Dir) -> i32 {
        match d {
            Dir::X => self.dx,
            Dir::Y => self.dy,
        }
    }
}

/// Pixels removed at the start and end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crop {
    /// At the start.
    pub start: i32,
    /// At the end.
    pub end: i32,
}

impl core::ops::Add for Crop {
    type Output = Crop;
    fn add(self, o: Crop) -> Crop {
        Crop {
            start: self.start + o.start,
            end: self.end + o.end,
        }
    }
}

/// A 1-D interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interval {
    /// Start.
    pub offset: i32,
    /// Length.
    pub length: i32,
}

impl Interval {
    /// An interval.
    pub const fn new(offset: i32, length: i32) -> Self {
        Self { offset, length }
    }
    /// Exclusive end.
    pub fn end(self) -> i32 {
        self.offset + self.length
    }
    pub(super) fn set_end(&mut self, end: i32) {
        self.length = end - self.offset;
    }
    pub(super) fn set_start(&mut self, start: i32) {
        self.length += self.offset - start;
        self.offset = start;
    }
    pub(super) fn contains(self, o: Interval) -> bool {
        self.offset <= o.offset && self.end() >= o.end()
    }
    pub(super) fn minus_crop(self, c: Crop) -> Interval {
        Interval::new(self.offset - c.start, self.length - c.start - c.end)
    }
    /// The crop that turns `self` into `inner`.
    pub(super) fn crop_to(self, inner: Interval) -> Crop {
        Crop {
            start: inner.offset - self.offset,
            end: self.end() - inner.end(),
        }
    }
    pub(super) fn include(&mut self, off: i32) {
        if off < self.offset {
            self.set_start(off);
        } else if off > self.end() {
            self.set_end(off);
        }
    }
}

/// Per-direction crops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crop2 {
    /// Horizontal.
    pub x: Crop,
    /// Vertical.
    pub y: Crop,
}

/// Per-direction intervals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interval2 {
    /// Horizontal.
    pub x: Interval,
    /// Vertical.
    pub y: Interval,
}

/// What one stage does to one tile: the input it reads, what it crops, the output it makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    /// Input window.
    pub input: Interval2,
    /// Crop applied.
    pub crop: Crop2,
    /// Output window.
    pub output: Interval2,
}

impl Region {
    pub(super) fn set(&mut self, d: Dir, input: Interval, crop: Crop, output: Interval) {
        match d {
            Dir::X => (self.input.x, self.crop.x, self.output.x) = (input, crop, output),
            Dir::Y => (self.input.y, self.crop.y, self.output.y) = (input, crop, output),
        }
    }
}

/// Output branches.
pub const NUM_BRANCHES: usize = 2;

/// A tile as the tiling sees it: one region per stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileRegions {
    /// Input stage.
    pub input: Region,
    /// Context stage.
    pub context: Region,
    /// Per-branch crop stage.
    pub crop: [Region; NUM_BRANCHES],
    /// Per-branch downscaler.
    pub downscale: [Region; NUM_BRANCHES],
    /// Per-branch resampler.
    pub resample: [Region; NUM_BRANCHES],
    /// Per-branch output.
    pub output: [Region; NUM_BRANCHES],
}

/// Inputs to the tiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TilingConfig {
    /// Input image size.
    pub input_image_size: Length2,
    /// Per-branch crop of the input.
    pub crop: [Interval2; NUM_BRANCHES],
    /// Per-branch downscaler output size.
    pub downscale_image_size: [Length2; NUM_BRANCHES],
    /// Per-branch output size (zero disables the branch).
    pub output_image_size: [Length2; NUM_BRANCHES],
    /// Maximum tile size.
    pub max_tile_size: Length2,
    /// Minimum tile size.
    pub min_tile_size: Length2,
    /// Per-branch downscale factors (4.12).
    pub downscale_factor: [Length2; NUM_BRANCHES],
    /// Per-branch resample factors (4.12).
    pub resample_factor: [Length2; NUM_BRANCHES],
    /// Per-branch horizontal mirroring.
    pub output_h_mirror: [bool; NUM_BRANCHES],
    /// Bit `i`: resampler `i` enabled.
    pub resample_enables: u32,
    /// Bit `i`: downscaler `i` enabled.
    pub downscale_enables: u32,
    /// Compressed input (8-pixel blocks).
    pub compressed_input: bool,
    /// Input alignment in pixels.
    pub input_alignment: Length2,
    /// Per-branch preferred output alignment.
    pub output_max_alignment: [Length2; NUM_BRANCHES],
    /// Per-branch required output alignment.
    pub output_min_alignment: [Length2; NUM_BRANCHES],
}

/// Tiling failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TilingError(pub String);

impl fmt::Display for TilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tiling failed: {}", self.0)
    }
}

impl core::error::Error for TilingError {}

pub(super) type Result<T> = core::result::Result<T, TilingError>;

pub(super) fn check(cond: bool, what: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(TilingError(what.into()))
    }
}

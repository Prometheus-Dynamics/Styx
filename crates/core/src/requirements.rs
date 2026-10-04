//! Pieces of a frame request that do not depend on a camera: regions of interest, output
//! formats and pyramid companions.
//!
//! The request itself, what a consumer wants from a camera (format, size, frame rate, how
//! frames are delivered), is `styx::planner::FrameRequest` in the `styx` crate, built with
//! `styx::prelude::Frames`: `Frames::nv12().size(1280, 800).fps(30).open(&camera)`. The planner
//! turns it into a concrete capture, decode and preparation plan for a device.
//!
//! [`FrameRequirements`] and [`Priority`] are the previous form of that request, kept for one
//! release as deprecated shims; `styx` converts them to a `FrameRequest`.

use alloc::vec::Vec;

use crate::format::FourCc;

mod legacy;
#[allow(deprecated)]
pub use legacy::{FrameRequirements, HardwarePolicy, PlanOverrides, Priority};

/// A rectangle in full-frame pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FrameRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl FrameRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// This rectangle clipped to a `width` x `height` frame; `None` if nothing is left.
    pub fn clipped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let right = self.x.saturating_add(self.width).min(width);
        let bottom = self.y.saturating_add(self.height).min(height);
        (right > x && bottom > y).then_some(Self::new(x, y, right - x, bottom - y))
    }

    /// The same region at `2^-level` scale (pyramid companion coordinates), rounded outward.
    pub fn scaled_down(self, level: u8) -> Self {
        let shift = u32::from(level.min(31));
        let x = self.x >> shift;
        let y = self.y >> shift;
        let right = (self.x + self.width).div_ceil(1 << shift);
        let bottom = (self.y + self.height).div_ceil(1 << shift);
        Self::new(x, y, right - x, bottom - y)
    }

    /// This region of a `from`-sized frame in a `to`-sized view of the same frame, rounded
    /// outward so it covers every pixel of the original.
    pub fn scaled(self, from: (u32, u32), to: (u32, u32)) -> Self {
        let (fw, fh) = (u64::from(from.0.max(1)), u64::from(from.1.max(1)));
        let (tw, th) = (u64::from(to.0), u64::from(to.1));
        let x = (u64::from(self.x) * tw / fw) as u32;
        let y = (u64::from(self.y) * th / fh) as u32;
        let right = (u64::from(self.x + self.width) * tw).div_ceil(fw) as u32;
        let bottom = (u64::from(self.y + self.height) * th).div_ceil(fh) as u32;
        Self::new(x, y, right - x, bottom - y)
    }
}

/// Pixel data the consumer wants.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OutputFormat {
    /// 8-bit luma (GREY). Zero-copy Y-plane views of NV12/YUV420 count.
    Luma,
    /// Any of these formats, most preferred first.
    Formats(Vec<FourCc>),
    /// Whatever the camera delivers, unconverted.
    Any,
}

impl OutputFormat {
    /// Whether `code` is this output without conversion.
    pub fn accepts(&self, code: FourCc) -> bool {
        match self {
            OutputFormat::Luma => matches!(code, FourCc::GREY | FourCc::R8),
            OutputFormat::Formats(formats) => formats.contains(&code),
            OutputFormat::Any => true,
        }
    }
}

/// Where pyramid levels may come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PyramidSource {
    /// An ISP/scaler output when available, else CPU box filters.
    #[default]
    PreferHardware,
    /// Fail planning unless hardware produces every level.
    HardwareOnly,
    /// Always CPU box filters.
    Software,
}

/// Pyramid companions to attach (`levels` = 2 means ½ and ¼): smaller copies of each frame
/// with the same timestamp, for consumers that search at several scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PyramidRequest {
    pub levels: u8,
    pub source: PyramidSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_clipping_and_pyramid_scaling() {
        let rect = FrameRect::new(100, 50, 300, 200);
        assert_eq!(
            rect.clipped_to(320, 240),
            Some(FrameRect::new(100, 50, 220, 190))
        );
        assert_eq!(rect.clipped_to(50, 50), None);
        assert_eq!(rect.scaled_down(1), FrameRect::new(50, 25, 150, 100));
        assert_eq!(
            FrameRect::new(3, 3, 2, 2).scaled_down(1),
            FrameRect::new(1, 1, 2, 2)
        );
    }

    #[test]
    fn output_formats_accept() {
        assert!(OutputFormat::Luma.accepts(FourCc::GREY));
        assert!(!OutputFormat::Luma.accepts(FourCc::NV12));
        assert!(OutputFormat::Formats(vec![FourCc::NV12]).accepts(FourCc::NV12));
        assert!(OutputFormat::Any.accepts(FourCc::MJPG));
    }
}

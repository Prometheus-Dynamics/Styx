//! Pixel and stream formats: what the bytes of a frame are.
//!
//! - [`FourCc`]: the one format code, V4L2-style four characters.
//! - [`FormatInfo`]: the facts of each known format (storage, packing, planes, chroma
//!   subsampling, sample depth, Bayer order, default colour), as one data table ([`FORMATS`]).
//! - [`ColorSpace`] and [`ColorRange`].
//! - [`plane_sizes`]: the stride and length of each plane of a frame.
//! - [`drm`]: the same formats as DRM fourccs and modifiers.
//! - [`Resolution`], [`Interval`], [`MediaFormat`].
//!
//! Conversions to and from V4L2, libcamera and Lemnos codes live with each backend, not here.
//! The crate is `no_std` and allocates nothing.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod color;
pub mod drm;
mod fourcc;
mod geometry;
mod info;
mod media;

pub use color::{ColorRange, ColorSpace};
pub use fourcc::{FourCc, ParseFourCcError};
pub use geometry::{
    LayoutError, MAX_PLANES, PlaneGeometry, PlaneSize, Planes, plane_geometry, plane_sizes,
};
pub use info::{
    BitDepth, CfaOrder, Channel, ChromaSubsampling, FORMATS, FormatInfo, PackedChannelOrder,
    PackedPixel, Storage,
};
pub use media::{Interval, IntervalStepwise, MediaFormat, Resolution};

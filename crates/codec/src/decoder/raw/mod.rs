//! Raw format decoders (pixel format conversions).

use styx_core::prelude::ColorSpace;
#[cfg(target_os = "linux")]
use styx_core::prelude::*;

#[cfg(target_os = "linux")]
use crate::CodecError;

mod bayer;
mod bgr;
mod bgra;
mod i420;
mod mono;
mod nv12;
mod passthrough;
mod rgb48;
mod rgba;
mod yuv;
mod yuv420p;
mod yuyv;

pub use bayer::{BayerToRgbDecoder, bayer_decoder_for, bayer_info};
pub use bgr::BgrToRgbDecoder;
pub use bgra::BgraToRgbDecoder;
pub use i420::I420ToRgbDecoder;
pub use mono::{Mono8ToRgbDecoder, Mono16ToRgbDecoder};
pub use nv12::{Nv12ToBgrDecoder, Nv12ToLumaDecoder, Nv12ToRgbDecoder};
pub use passthrough::PassthroughDecoder;
pub use rgb48::Rgb48ToRgbDecoder;
pub use rgba::RgbaToRgbDecoder;
pub use yuv::{NvToRgbDecoder, Packed422ToRgbDecoder, PlanarYuvToRgbDecoder};
pub use yuv420p::Yuv420pToRgbDecoder;
pub use yuyv::{YuyvToLumaDecoder, YuyvToRgbDecoder};

#[cfg(target_os = "linux")]
pub trait RawDecodeInto {
    fn output_bytes_per_pixel(&self) -> usize;
    fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError>;
}

#[cfg(target_os = "linux")]
pub trait SharedRawDecodeExt: RawDecodeInto {
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<FrameLease, CodecError> {
        let layout = plane_layout_from_dims(
            input.meta().format.resolution.width,
            input.meta().format.resolution.height,
            self.output_bytes_per_pixel(),
        );
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(layout.len)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        let meta = self.decode_into(input, lease.as_mut_slice())?;
        FrameLease::single_plane_shared(meta, lease, layout.len, layout.stride)
            .map_err(|err| CodecError::Codec(err.to_string()))
    }
}

#[cfg(target_os = "linux")]
impl<T: RawDecodeInto + ?Sized> SharedRawDecodeExt for T {}

#[cfg(target_os = "linux")]
macro_rules! impl_raw_decode_into {
    ($ty:ty, $bpp:expr) => {
        impl RawDecodeInto for $ty {
            fn output_bytes_per_pixel(&self) -> usize {
                $bpp
            }

            fn decode_into(
                &self,
                input: &FrameLease,
                dst: &mut [u8],
            ) -> Result<FrameMeta, CodecError> {
                <$ty>::decode_into(self, input, dst)
            }
        }
    };
}

#[cfg(target_os = "linux")]
impl_raw_decode_into!(BayerToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(BgrToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(BgraToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(I420ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Mono8ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Mono16ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Nv12ToBgrDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Nv12ToLumaDecoder, 1);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Nv12ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(NvToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Packed422ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(PlanarYuvToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Rgb48ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(RgbaToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(Yuv420pToRgbDecoder, 3);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(YuyvToLumaDecoder, 1);
#[cfg(target_os = "linux")]
impl_raw_decode_into!(YuyvToRgbDecoder, 3);

#[derive(Clone, Copy)]
struct YuvCoeffs {
    r_v: i32,
    g_u: i32,
    g_v: i32,
    b_u: i32,
    full_range: bool,
}

const BT709: YuvCoeffs = YuvCoeffs {
    r_v: 459,
    g_u: 55,
    g_v: 136,
    b_u: 541,
    full_range: false,
};

// Full-range Rec.601 coefficients (Y range 0..255).
const BT601_FULL: YuvCoeffs = YuvCoeffs {
    r_v: 359,
    g_u: 88,
    g_v: 183,
    b_u: 454,
    full_range: true,
};

const BT2020: YuvCoeffs = YuvCoeffs {
    r_v: 430,
    g_u: 48,
    g_v: 166,
    b_u: 549,
    full_range: false,
};

/// Integer conversion with clamping using limited-range YUV coefficients.
#[inline(always)]
pub(crate) fn yuv_to_rgb(y: i32, u: i32, v: i32, color: ColorSpace) -> (u8, u8, u8) {
    let coeffs = match color {
        ColorSpace::Bt709 => BT709,
        ColorSpace::Bt2020 => BT2020,
        // `Srgb` in our metadata means "full-range output" (libcamera frequently reports sYCC).
        // libcamera's sYCC uses a Rec.601 YCbCr matrix with full-range.
        ColorSpace::Srgb => BT601_FULL,
        // Default unknown to limited-range Rec.709; BT.601 assumptions tend to skew heavily.
        ColorSpace::Unknown => BT709,
    };
    let d = u - 128;
    let e = v - 128;
    let (c, scale) = if coeffs.full_range {
        (y.max(0), 256)
    } else {
        (y.saturating_sub(16).max(0), 298)
    };
    let r = (scale * c + coeffs.r_v * e + 128) >> 8;
    let g = (scale * c - coeffs.g_u * d - coeffs.g_v * e + 128) >> 8;
    let b = (scale * c + coeffs.b_u * d + 128) >> 8;
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

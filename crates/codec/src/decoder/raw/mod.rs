//! Raw format decoders (pixel format conversions).

use styx_core::prelude::*;

#[cfg(any(feature = "raw-decoders", target_os = "linux"))]
use crate::CodecError;
use crate::{CodecDescriptor, CodecKind};

/// The YUV range and matrix the decoders convert a frame labelled `color` with. `Srgb` YUV
/// is full-range BT.601 (sYCC: JFIF, the native ISPs' output, libcamera's full-range spaces);
/// `Unknown` is far more likely limited-range BT.709 than full-range BT.601.
#[cfg(any(feature = "image", feature = "raw-decoders"))]
pub(crate) fn yuv_colorspace(
    color: ColorSpace,
) -> (yuvutils_rs::YuvRange, yuvutils_rs::YuvStandardMatrix) {
    use yuvutils_rs::{YuvRange, YuvStandardMatrix};
    match color {
        ColorSpace::Bt709 | ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
        ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
    }
}

#[cfg(feature = "raw-decoders")]
mod bayer;
#[cfg(feature = "raw-decoders")]
mod bgr;
#[cfg(feature = "raw-decoders")]
mod bgra;
#[cfg(feature = "raw-decoders")]
mod i420;
#[cfg(feature = "raw-decoders")]
mod mono;
#[cfg(feature = "raw-decoders")]
mod nv12;
mod passthrough;
#[cfg(feature = "raw-decoders")]
mod rgb48;
#[cfg(feature = "raw-decoders")]
mod rgba;
#[cfg(feature = "raw-decoders")]
mod yuv;
#[cfg(feature = "raw-decoders")]
mod yuv420p;
#[cfg(feature = "raw-decoders")]
mod yuyv;
#[cfg(feature = "raw-decoders")]
mod yuyv_nv12;

#[cfg(feature = "raw-decoders")]
pub use bayer::{SoftIspDecoder, SoftIspOutput, bayer_decoders_for};
#[cfg(feature = "raw-decoders")]
pub use bgr::BgrToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use bgra::BgraToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use i420::I420ToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use mono::{Mono8ToRgbDecoder, Mono16ToRgbDecoder};
#[cfg(feature = "raw-decoders")]
pub use nv12::{Nv12ToBgrDecoder, Nv12ToLumaDecoder, Nv12ToRgbDecoder};
pub use passthrough::PassthroughDecoder;
#[cfg(feature = "raw-decoders")]
pub use rgb48::Rgb48ToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use rgba::RgbaToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use yuv::{NvToRgbDecoder, Packed422ToRgbDecoder, PlanarYuvToRgbDecoder};
#[cfg(feature = "raw-decoders")]
pub use yuv420p::Yuv420pToRgbDecoder;
#[cfg(feature = "raw-decoders")]
pub use yuyv::{YuyvToLumaDecoder, YuyvToRgbDecoder};
#[cfg(feature = "raw-decoders")]
pub use yuyv_nv12::YuyvToNv12Decoder;

pub(crate) fn raw_decoder_descriptor(
    input: FourCc,
    output: FourCc,
    name: &'static str,
    impl_name: &'static str,
) -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Decoder,
        input,
        output,
        name,
        impl_name,
    }
}

#[cfg(feature = "raw-decoders")]
pub(crate) fn process_owned_raw_decode(
    input: FrameLease,
    pool: &BufferPool,
    output_bytes_per_pixel: usize,
    decode: impl FnOnce(&FrameLease, &mut [u8]) -> Result<FrameMeta, CodecError>,
) -> Result<FrameLease, CodecError> {
    let layout = plane_layout_from_dims(
        input.meta().format.resolution.width,
        input.meta().format.resolution.height,
        output_bytes_per_pixel,
    );
    let mut buf = pool.lease();
    unsafe { buf.resize_uninit(layout.len) };
    let meta = decode(&input, buf.as_mut_slice())?;
    Ok(unsafe { FrameLease::single_plane_uninit(meta, buf, layout.len, layout.stride) })
}

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

#[cfg(all(target_os = "linux", feature = "raw-decoders"))]
pub(crate) fn process_shared_raw_decode<T>(
    decoder: &T,
    input: &FrameLease,
    pool: &SharedBufferPool,
) -> Result<Option<FrameLease>, CodecError>
where
    T: SharedRawDecodeExt + ?Sized,
{
    SharedRawDecodeExt::process_shared(decoder, input, pool).map(Some)
}

#[cfg(feature = "raw-decoders")]
pub(crate) fn decode_strided_rows_to_rgb24(
    src: &[u8],
    dst: &mut [u8],
    row_count: usize,
    src_stride: usize,
    src_row_bytes: usize,
    dst_row_bytes: usize,
    convert_row: impl Fn(&[u8], &mut [u8]) + Sync,
) {
    let len = row_count.saturating_mul(dst_row_bytes).min(dst.len());
    crate::par::for_each_row(&mut dst[..len], dst_row_bytes, |y, dst_line| {
        let src_line = &src[y * src_stride..][..src_row_bytes];
        convert_row(src_line, dst_line);
    });
}

#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
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
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(BgrToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(BgraToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(I420ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Mono8ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Mono16ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Nv12ToBgrDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Nv12ToLumaDecoder, 1);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Nv12ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(NvToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Packed422ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(PlanarYuvToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Rgb48ToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(RgbaToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(Yuv420pToRgbDecoder, 3);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(YuyvToLumaDecoder, 1);
#[cfg(target_os = "linux")]
#[cfg(feature = "raw-decoders")]
impl_raw_decode_into!(YuyvToRgbDecoder, 3);

/// Integer conversion with clamping using limited-range YUV coefficients.
#[inline(always)]
#[cfg(any(feature = "raw-decoders", feature = "image"))]
pub(crate) fn yuv_to_rgb(y: i32, u: i32, v: i32, color: ColorSpace) -> (u8, u8, u8) {
    let [r, g, b] = styx_core::simd::YuvMatrix::for_color(color).rgb(
        y.clamp(0, 255) as u8,
        u.clamp(0, 255) as u8,
        v.clamp(0, 255) as u8,
    );
    (r, g, b)
}

/// Whether NV12, I420 and YUYV convert with Styx's own rows rather than yuvutils-rs: on the
/// targets yuvutils-rs has no vector code for (32-bit Arm, RISC-V), where Styx's NEON and RVV
/// leaves run.
#[cfg(feature = "raw-decoders")]
pub(crate) const STYX_YUV_ROWS: bool = cfg!(any(target_arch = "arm", target_arch = "riscv64"));

//! `pisp_common.h`: image formats, black level, white balance, (de)compression, AXI.

use bytemuck::{Pod, Zeroable};

/// Bayer orders (`enum pisp_bayer_order`). Bit 0: G on the even pixels; bit 1: R/B swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BayerOrder {
    /// RGGB.
    Rggb = 0,
    /// GBRG.
    Gbrg = 1,
    /// BGGR.
    Bggr = 2,
    /// GRBG.
    Grbg = 3,
    /// Monochrome (the other bits are ignored).
    Greyscale = 128,
}

/// `enum pisp_image_format` bits. A format is an OR of one value from each group.
pub mod image_format {
    /// 8 bits per sample.
    pub const BPS_8: u32 = 0x0000_0000;
    /// 10 bits per sample (packed 3 samples in 4 bytes, PiSP's own packing).
    pub const BPS_10: u32 = 0x0000_0001;
    /// 12 bits per sample.
    pub const BPS_12: u32 = 0x0000_0002;
    /// 16 bits per sample.
    pub const BPS_16: u32 = 0x0000_0003;
    /// Bits-per-sample mask.
    pub const BPS_MASK: u32 = 0x0000_0003;
    /// Interleaved samples.
    pub const PLANARITY_INTERLEAVED: u32 = 0x0000_0000;
    /// Luma plane plus an interleaved chroma plane.
    pub const PLANARITY_SEMI_PLANAR: u32 = 0x0000_0010;
    /// One plane per channel.
    pub const PLANARITY_PLANAR: u32 = 0x0000_0020;
    /// Planarity mask.
    pub const PLANARITY_MASK: u32 = 0x0000_0030;
    /// 4:4:4 sampling.
    pub const SAMPLING_444: u32 = 0x0000_0000;
    /// 4:2:2 sampling.
    pub const SAMPLING_422: u32 = 0x0000_0100;
    /// 4:2:0 sampling.
    pub const SAMPLING_420: u32 = 0x0000_0200;
    /// Sampling mask.
    pub const SAMPLING_MASK: u32 = 0x0000_0300;
    /// Channel order swapped (e.g. NV21 rather than NV12, BGR rather than RGB).
    pub const ORDER_SWAPPED: u32 = 0x0000_1000;
    /// Left shift of samples (0..8) in `SHIFT_1` units.
    pub const SHIFT_1: u32 = 0x0001_0000;
    /// Shift mask.
    pub const SHIFT_MASK: u32 = 0x000f_0000;
    /// 32 bits per pixel (RGBX).
    pub const BPP_32: u32 = 0x0010_0000;
    /// Write the X (alpha) value.
    pub const X_VALUE: u32 = 0x0020_0000;
    /// Compression mode 1.
    pub const COMPRESSION_MODE_1: u32 = 0x0100_0000;
    /// Compression mode 2.
    pub const COMPRESSION_MODE_2: u32 = 0x0200_0000;
    /// Compression mode 3.
    pub const COMPRESSION_MODE_3: u32 = 0x0300_0000;
    /// Compression mask.
    pub const COMPRESSION_MASK: u32 = 0x0300_0000;
    /// Signed HOG output.
    pub const HOG_SIGNED: u32 = 0x0400_0000;
    /// Unsigned HOG output.
    pub const HOG_UNSIGNED: u32 = 0x0800_0000;
    /// Integral image output.
    pub const INTEGRAL_IMAGE: u32 = 0x1000_0000;
    /// "Wallpaper" (column-rolled) layout.
    pub const WALLPAPER_ROLL: u32 = 0x2000_0000;
    /// Three channels (colour); absent means one channel (Bayer or mono).
    pub const THREE_CHANNEL: u32 = 0x4000_0000;

    /// Width of a wallpaper roll in bytes.
    pub const WALLPAPER_WIDTH: u32 = 128;

    /// Bits per sample: 8, 10, 12 or 16.
    pub fn bits_per_sample(fmt: u32) -> u32 {
        match fmt & BPS_MASK {
            BPS_10 => 10,
            BPS_12 => 12,
            BPS_16 => 16,
            _ => 8,
        }
    }

    /// Whether the format is compressed.
    pub fn is_compressed(fmt: u32) -> bool {
        fmt & COMPRESSION_MASK != 0
    }

    /// Whether the format has three channels.
    pub fn is_three_channel(fmt: u32) -> bool {
        fmt & THREE_CHANNEL != 0
    }
}

/// `struct pisp_image_format_config`: an image's size, format and strides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct ImageFormatConfig {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// [`image_format`] bits.
    pub format: u32,
    /// Line stride in bytes (first plane).
    pub stride: i32,
    /// Line stride in bytes of the second (and third) plane.
    pub stride2: i32,
}

/// `struct pisp_bla_config`: black level subtraction per Bayer channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BlaConfig {
    /// Red black level (16-bit scale).
    pub black_level_r: u16,
    /// Gr black level.
    pub black_level_gr: u16,
    /// Gb black level.
    pub black_level_gb: u16,
    /// Blue black level.
    pub black_level_b: u16,
    /// Black level left in the output.
    pub output_black_level: u16,
    /// Padding.
    pub pad: [u8; 2],
}

/// `struct pisp_wbg_config`: white balance gains (unsigned 4.10 fixed point, 1024 = 1.0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct WbgConfig {
    /// Red gain.
    pub gain_r: u16,
    /// Green gain.
    pub gain_g: u16,
    /// Blue gain.
    pub gain_b: u16,
    /// Padding.
    pub pad: [u8; 2],
}

/// `struct pisp_compress_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CompressConfig {
    /// Value subtracted from the incoming data.
    pub offset: u16,
    /// Padding.
    pub pad: u8,
    /// 1 companding, 2 delta (recommended), 3 combined (HDR).
    pub mode: u8,
}

/// `struct pisp_decompress_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct DecompressConfig {
    /// Value added to the reconstructed data.
    pub offset: u16,
    /// Padding.
    pub pad: u8,
    /// 1 companding, 2 delta (recommended), 3 combined (HDR).
    pub mode: u8,
}

/// `enum pisp_axi_flags`: round bursts down to end on a 32-byte boundary.
pub const AXI_FLAG_ALIGN: u8 = 128;
/// FE writer: pad output to a 16-byte boundary.
pub const AXI_FLAG_PAD: u8 = 64;
/// FE writer: use the output FIFO level to trigger "panic".
pub const AXI_FLAG_PANIC: u8 = 32;

/// `struct pisp_axi_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct AxiConfig {
    /// Burst length minus one (0..15) OR'd with `AXI_FLAG_*`.
    pub maxlen_flags: u8,
    /// `{ prot[2:0], cache[3:0] }`.
    pub cache_prot: u8,
    /// QoS.
    pub qos: u16,
}

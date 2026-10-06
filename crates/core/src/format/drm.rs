//! Styx [`FourCc`]s (V4L2-style codes) as DRM format codes and modifiers, and back.
//!
//! What a frame is called where the rest of Linux graphics looks at it (KMS, EGL/Vulkan dma-buf
//! import, GStreamer `DMA_DRM` caps, Daedalus's `daedalus:frame` view). Plain `const` tables,
//! `no_std`, nothing allocated.
//!
//! - [`to_drm`]: a Styx format as a [`DrmFormat`] (fourcc + modifier), `None` when DRM has no
//!   format with the same bytes (compressed MJPEG / JPEG / H.264 / H.265, and anything not
//!   listed).
//! - [`from_drm`]: a DRM fourcc and modifier as the Styx format, `None` when Styx has none.
//! - [`MAPPINGS`]: the table itself, one row per DRM format; [`ALIASES`] lists the Styx codes
//!   that name the same bytes as a row's code.
//!
//! The mapping is by memory layout, byte for byte; codes that look alike do not always mean
//! the same thing:
//!
//! - **Packed RGB.** DRM names packed formats by the channels of a little-endian word from its
//!   most significant end, V4L2 (and Styx) mostly in memory order. Styx `RG24` (= V4L2 `RGB3`,
//!   bytes R, G, B) is DRM `BG24` (`DRM_FORMAT_BGR888`), and Styx `BG24` (= `BGR3`, bytes B, G,
//!   R) is DRM `RG24` (`RGB888`). Likewise `RGBA` is DRM `AB24` (`ABGR8888`), `BGRA` is `AR24`
//!   (`ARGB8888`), `RG48` (= V4L2 `RGB6`, R, G, B in 16-bit little-endian words) is `BG48`
//!   (`BGR161616`) and `BG48` is `RG48`. The 32-bit `XR24` / `XB24` are V4L2's own codes, which
//!   V4L2 defined to match DRM (`XR24`: bytes B, G, R, x), so they map to themselves.
//! - **Greyscale.** `GREY` is DRM `GREY` (`DRM_FORMAT_Y8`). DRM has no wider luma-only formats;
//!   10/12/16-bit greyscale in 16-bit little-endian words (`Y10 `, `Y12 `, `Y16 `) is byte for
//!   byte the single-channel `R10` / `R12` / `R16`, and Styx `R8` / `R16 ` are those directly.
//!   `D32F` (32-bit float depth) is `R  F` (`DRM_FORMAT_R32F`): the same bytes, without the
//!   depth meaning.
//! - **Bayer and CSI-2 packed raw.** The kernel's `drm_fourcc.h` has no Bayer formats. These
//!   rows follow libcamera's extension of it ([`DrmRegistry::Libcamera`]): the V4L2 code of the
//!   unpacked format (one sample per little-endian 16-bit word above 8 bits), with
//!   [`MIPI_FORMAT_MOD_CSI2_PACKED`] for the CSI-2 packed layouts (`pBAA`, `pBCC`, `Y10P`, ...).
//!   Two of libcamera's codes differ from V4L2's: 14-bit GRBG is `BA14` (V4L2 `GR14`) and 16-bit
//!   RGGB is `RGB6` (V4L2 `RG16`) - the same code as V4L2's 48-bit RGB `RGB6`, which is DRM `BG48`
//!   here. libcamera's MIPI modifier vendor (`0x0b`) is also the kernel's MediaTek vendor; it is
//!   only meaningful together with a Bayer or greyscale code. Consumers that only know the
//!   kernel's formats can skip these rows ([`DrmFormat::registry`]).
//! - **Compressed** (`MJPG`, `JPEG`, `H264`, `H265`, `HEVC`): no DRM format, `None`.
//! - **Not mapped:** 14-bit greyscale (`Y14 `; DRM and libcamera have no `R14`), and any code not
//!   in the tables.
//!
//! [`from_drm`] wants the modifier exactly: [`DRM_FORMAT_MOD_LINEAR`] for linear rows (a
//! producer's [`DRM_FORMAT_MOD_INVALID`] says the layout is implicit, so it is not taken for
//! linear), [`MIPI_FORMAT_MOD_CSI2_PACKED`] for packed raw. Big-endian codes
//! (`DRM_FORMAT_BIG_ENDIAN`) are never mapped.

use super::FourCc;

/// DRM fourcc code of a four-character format name, e.g. `drm_fourcc(b"NV12")`
/// (`fourcc_code` in `drm_fourcc.h`).
pub const fn drm_fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// `DRM_FORMAT_INVALID`: no format.
pub const DRM_FORMAT_INVALID: u32 = 0;
/// `DRM_FORMAT_BIG_ENDIAN`: the flag of big-endian variants (never mapped here).
pub const DRM_FORMAT_BIG_ENDIAN: u32 = 1 << 31;
/// `DRM_FORMAT_MOD_LINEAR`: rows one after the other, no tiling or compression.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: no explicit modifier (layout implied by the producer).
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// libcamera's `MIPI_FORMAT_MOD_CSI2_PACKED` (vendor `0x0b`, value 1): MIPI CSI-2 packing of
/// 10-, 12- and 14-bit samples (RAW10: 4 samples in 5 bytes, RAW12: 2 in 3).
pub const MIPI_FORMAT_MOD_CSI2_PACKED: u64 = (0x0b << 56) | 1;

/// Which `drm_fourcc.h` defines a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrmRegistry {
    /// The kernel's (`include/uapi/drm/drm_fourcc.h`).
    Kernel,
    /// libcamera's extension of it (Bayer formats and the MIPI CSI-2 packing modifier).
    Libcamera,
}

/// A DRM format: fourcc and modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrmFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

impl DrmFormat {
    /// A linear format.
    pub const fn linear(code: &[u8; 4]) -> Self {
        Self {
            fourcc: drm_fourcc(code),
            modifier: DRM_FORMAT_MOD_LINEAR,
        }
    }

    /// A format in MIPI CSI-2 packing.
    pub const fn csi2_packed(code: &[u8; 4]) -> Self {
        Self {
            fourcc: drm_fourcc(code),
            modifier: MIPI_FORMAT_MOD_CSI2_PACKED,
        }
    }

    /// Which `drm_fourcc.h` defines the format, `None` when it is not in [`MAPPINGS`].
    pub fn registry(self) -> Option<DrmRegistry> {
        MAPPINGS.iter().find(|m| m.drm == self).map(|m| m.registry)
    }
}

/// One row of the table: a Styx format and the DRM format with the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrmMapping {
    pub fourcc: FourCc,
    pub drm: DrmFormat,
    pub registry: DrmRegistry,
}

const fn kernel(styx: &[u8; 4], drm: &[u8; 4]) -> DrmMapping {
    DrmMapping {
        fourcc: FourCc::new(*styx),
        drm: DrmFormat::linear(drm),
        registry: DrmRegistry::Kernel,
    }
}

const fn bayer(styx: &[u8; 4], drm: &[u8; 4]) -> DrmMapping {
    DrmMapping {
        fourcc: FourCc::new(*styx),
        drm: DrmFormat::linear(drm),
        registry: DrmRegistry::Libcamera,
    }
}

const fn csi2(styx: &[u8; 4], drm: &[u8; 4]) -> DrmMapping {
    DrmMapping {
        fourcc: FourCc::new(*styx),
        drm: DrmFormat::csi2_packed(drm),
        registry: DrmRegistry::Libcamera,
    }
}

/// Every Styx format with a DRM equivalent, one row per DRM format (the Styx code [`from_drm`]
/// gives back).
pub const MAPPINGS: &[DrmMapping] = &[
    // Greyscale and single-channel.
    kernel(b"GREY", b"GREY"),
    kernel(b"R8  ", b"R8  "),
    kernel(b"Y10 ", b"R10 "),
    kernel(b"Y12 ", b"R12 "),
    kernel(b"R16 ", b"R16 "),
    kernel(b"D32F", b"R  F"),
    // Packed RGB: memory order on the Styx side, little-endian word order on the DRM side.
    kernel(b"RG24", b"BG24"),
    kernel(b"BG24", b"RG24"),
    kernel(b"RGBA", b"AB24"),
    kernel(b"BGRA", b"AR24"),
    kernel(b"XR24", b"XR24"),
    kernel(b"XB24", b"XB24"),
    kernel(b"RG48", b"BG48"),
    kernel(b"BG48", b"RG48"),
    // Packed YUV 4:2:2.
    kernel(b"YUYV", b"YUYV"),
    kernel(b"YVYU", b"YVYU"),
    kernel(b"UYVY", b"UYVY"),
    kernel(b"VYUY", b"VYUY"),
    // Semi-planar YUV.
    kernel(b"NV12", b"NV12"),
    kernel(b"NV21", b"NV21"),
    kernel(b"NV16", b"NV16"),
    kernel(b"NV61", b"NV61"),
    kernel(b"NV24", b"NV24"),
    kernel(b"NV42", b"NV42"),
    // Planar YUV.
    kernel(b"YU12", b"YU12"),
    kernel(b"YV12", b"YV12"),
    kernel(b"YU16", b"YU16"),
    kernel(b"YV16", b"YV16"),
    kernel(b"YU24", b"YU24"),
    kernel(b"YV24", b"YV24"),
    // Bayer, one sample per byte or per little-endian 16-bit word (libcamera).
    bayer(b"RGGB", b"RGGB"),
    bayer(b"GRBG", b"GRBG"),
    bayer(b"GBRG", b"GBRG"),
    bayer(b"BA81", b"BA81"),
    bayer(b"RG10", b"RG10"),
    bayer(b"BA10", b"BA10"),
    bayer(b"GB10", b"GB10"),
    bayer(b"BG10", b"BG10"),
    bayer(b"RG12", b"RG12"),
    bayer(b"BA12", b"BA12"),
    bayer(b"GB12", b"GB12"),
    bayer(b"BG12", b"BG12"),
    bayer(b"RG14", b"RG14"),
    bayer(b"GR14", b"BA14"),
    bayer(b"GB14", b"GB14"),
    bayer(b"BG14", b"BG14"),
    bayer(b"RG16", b"RGB6"),
    bayer(b"GR16", b"GR16"),
    bayer(b"GB16", b"GB16"),
    bayer(b"BYR2", b"BYR2"),
    // MIPI CSI-2 packed raw (libcamera's unpacked code + the CSI-2 modifier).
    csi2(b"pRAA", b"RG10"),
    csi2(b"pgAA", b"BA10"),
    csi2(b"pGAA", b"GB10"),
    csi2(b"pBAA", b"BG10"),
    csi2(b"pRCC", b"RG12"),
    csi2(b"pgCC", b"BA12"),
    csi2(b"pGCC", b"GB12"),
    csi2(b"pBCC", b"BG12"),
    csi2(b"Y10P", b"R10 "),
    csi2(b"Y12P", b"R12 "),
];

/// Styx codes for the same bytes as another code in [`MAPPINGS`]: `(alias, code)`.
/// [`to_drm`] maps them as their code; [`from_drm`] gives the code.
pub const ALIASES: &[(FourCc, FourCc)] = &[
    (FourCc::new(*b"Y16 "), FourCc::R16),
    (FourCc::RGB3, FourCc::RG24),
    (FourCc::BGR3, FourCc::BG24),
    (FourCc::new(*b"RGB6"), FourCc::RG48),
    (FourCc::I420, FourCc::YU12),
    (FourCc::BGGR, FourCc::new(*b"BA81")),
    (FourCc::new(*b"GR10"), FourCc::new(*b"BA10")),
    (FourCc::new(*b"GR12"), FourCc::new(*b"BA12")),
    (FourCc::new(*b"BA14"), FourCc::new(*b"GR14")),
    (FourCc::new(*b"BG16"), FourCc::new(*b"BYR2")),
];

/// Styx codes that have no DRM format: [`to_drm`] is `None` for them (as for any code not in
/// [`MAPPINGS`] or [`ALIASES`]).
pub const UNMAPPED: &[FourCc] = &[
    FourCc::MJPG,
    FourCc::JPEG,
    FourCc::H264,
    FourCc::H265,
    FourCc::HEVC,
    FourCc::new(*b"Y14 "),
];

/// The row for `code` (or for the code it is an alias of).
pub fn mapping(code: FourCc) -> Option<&'static DrmMapping> {
    let code = ALIASES
        .iter()
        .find(|(alias, _)| *alias == code)
        .map_or(code, |&(_, code)| code);
    MAPPINGS.iter().find(|m| m.fourcc == code)
}

/// `code` as a DRM format, `None` when DRM has no format with the same bytes (see the module).
pub fn to_drm(code: FourCc) -> Option<DrmFormat> {
    mapping(code).map(|m| m.drm)
}

/// The Styx format of DRM `fourcc` with `modifier`, `None` when Styx has none (or the modifier
/// is not the row's: see the module).
pub fn from_drm(fourcc: u32, modifier: u64) -> Option<FourCc> {
    let drm = DrmFormat { fourcc, modifier };
    MAPPINGS.iter().find(|m| m.drm == drm).map(|m| m.fourcc)
}

#[cfg(test)]
mod tests;

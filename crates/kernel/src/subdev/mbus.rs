//! Media bus codes (`MEDIA_BUS_FMT_*`, linux/media-bus-format.h): the formats on the links
//! between subdevices.

use std::fmt;

/// A media bus format code, e.g. `SRGGB10_1X10` (0x300f).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MbusCode(pub u32);

impl MbusCode {
    /// 8-bit greyscale.
    pub const Y8_1X8: Self = Self(0x2001);
    /// 10-bit greyscale.
    pub const Y10_1X10: Self = Self(0x200a);
    /// 12-bit greyscale.
    pub const Y12_1X12: Self = Self(0x2013);
    /// Packed YUYV 4:2:2 over an 8-bit bus.
    pub const YUYV8_2X8: Self = Self(0x2008);
    /// Packed UYVY 4:2:2 over a 16-bit bus.
    pub const UYVY8_1X16: Self = Self(0x200f);
    /// 10-bit Bayer RGGB.
    pub const SRGGB10_1X10: Self = Self(0x300f);
    /// 10-bit Bayer BGGR.
    pub const SBGGR10_1X10: Self = Self(0x3007);
    /// Sensor embedded data, 8-bit.
    pub const META_8: Self = Self(0x8001);

    /// The code's name without the `MEDIA_BUS_FMT_` prefix, when known.
    pub fn name(self) -> Option<&'static str> {
        NAMES
            .binary_search_by_key(&self.0, |&(v, _)| v)
            .ok()
            .map(|i| NAMES[i].1)
    }

    /// Looks a code up by name (with or without the `MEDIA_BUS_FMT_` prefix).
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.strip_prefix("MEDIA_BUS_FMT_").unwrap_or(name);
        NAMES
            .iter()
            .find(|&&(_, n)| n == name)
            .map(|&(v, _)| Self(v))
    }
}

impl fmt::Debug for MbusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => write!(f, "{n}"),
            None => write!(f, "{:#06x}", self.0),
        }
    }
}

impl fmt::Display for MbusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Known codes, sorted by value.
static NAMES: &[(u32, &str)] = &[
    (0x0001, "FIXED"),
    (0x1001, "RGB444_2X8_PADHI_BE"),
    (0x1002, "RGB444_2X8_PADHI_LE"),
    (0x1003, "RGB555_2X8_PADHI_BE"),
    (0x1004, "RGB555_2X8_PADHI_LE"),
    (0x1005, "BGR565_2X8_BE"),
    (0x1006, "BGR565_2X8_LE"),
    (0x1007, "RGB565_2X8_BE"),
    (0x1008, "RGB565_2X8_LE"),
    (0x1009, "RGB666_1X18"),
    (0x100a, "RGB888_1X24"),
    (0x100b, "RGB888_2X12_BE"),
    (0x100c, "RGB888_2X12_LE"),
    (0x100d, "ARGB8888_1X32"),
    (0x100e, "RBG888_1X24"),
    (0x100f, "RGB888_1X32_PADHI"),
    (0x1010, "RGB666_1X7X3_SPWG"),
    (0x1011, "RGB888_1X7X4_SPWG"),
    (0x1012, "RGB888_1X7X4_JEIDA"),
    (0x1013, "BGR888_1X24"),
    (0x1014, "GBR888_1X24"),
    (0x1015, "RGB666_1X24_CPADHI"),
    (0x1016, "RGB444_1X12"),
    (0x1017, "RGB565_1X16"),
    (0x1018, "RGB101010_1X30"),
    (0x1019, "RGB121212_1X36"),
    (0x101a, "RGB161616_1X48"),
    (0x101b, "BGR888_3X8"),
    (0x101c, "RGB888_3X8"),
    (0x101d, "RGB888_3X8_DELTA"),
    (0x101e, "RGB666_1X30_CPADLO"),
    (0x101f, "RGB888_1X30_CPADLO"),
    (0x1020, "RGB666_1X36_CPADLO"),
    (0x1021, "RGB888_1X36_CPADLO"),
    (0x1022, "RGB565_1X24_CPADHI"),
    (0x1023, "BGR666_1X18"),
    (0x1024, "BGR666_1X24_CPADHI"),
    (0x1025, "RGB666_2X9_BE"),
    (0x1026, "RGB101010_1X7X5_SPWG"),
    (0x1027, "RGB101010_1X7X5_JEIDA"),
    (0x1028, "RGB202020_1X60"),
    (0x2001, "Y8_1X8"),
    (0x2002, "UYVY8_1_5X8"),
    (0x2003, "VYUY8_1_5X8"),
    (0x2004, "YUYV8_1_5X8"),
    (0x2005, "YVYU8_1_5X8"),
    (0x2006, "UYVY8_2X8"),
    (0x2007, "VYUY8_2X8"),
    (0x2008, "YUYV8_2X8"),
    (0x2009, "YVYU8_2X8"),
    (0x200a, "Y10_1X10"),
    (0x200b, "YUYV10_2X10"),
    (0x200c, "YVYU10_2X10"),
    (0x200d, "YUYV10_1X20"),
    (0x200e, "YVYU10_1X20"),
    (0x200f, "UYVY8_1X16"),
    (0x2010, "VYUY8_1X16"),
    (0x2011, "YUYV8_1X16"),
    (0x2012, "YVYU8_1X16"),
    (0x2013, "Y12_1X12"),
    (0x2014, "YDYUYDYV8_1X16"),
    (0x2015, "UV8_1X8"),
    (0x2016, "YUV10_1X30"),
    (0x2017, "AYUV8_1X32"),
    (0x2018, "UYVY10_2X10"),
    (0x2019, "VYUY10_2X10"),
    (0x201a, "UYVY10_1X20"),
    (0x201b, "VYUY10_1X20"),
    (0x201c, "UYVY12_2X12"),
    (0x201d, "VYUY12_2X12"),
    (0x201e, "YUYV12_2X12"),
    (0x201f, "YVYU12_2X12"),
    (0x2020, "UYVY12_1X24"),
    (0x2021, "VYUY12_1X24"),
    (0x2022, "YUYV12_1X24"),
    (0x2023, "YVYU12_1X24"),
    (0x2024, "VUY8_1X24"),
    (0x2025, "YUV8_1X24"),
    (0x2026, "UYYVYY8_0_5X24"),
    (0x2027, "UYYVYY10_0_5X30"),
    (0x2028, "UYYVYY12_0_5X36"),
    (0x2029, "YUV12_1X36"),
    (0x202a, "YUV16_1X48"),
    (0x202b, "UYYVYY16_0_5X48"),
    (0x202c, "Y10_2X8_PADHI_LE"),
    (0x202d, "Y14_1X14"),
    (0x202e, "Y16_1X16"),
    (0x3001, "SBGGR8_1X8"),
    (0x3002, "SGRBG8_1X8"),
    (0x3003, "SBGGR10_2X8_PADHI_BE"),
    (0x3004, "SBGGR10_2X8_PADHI_LE"),
    (0x3005, "SBGGR10_2X8_PADLO_BE"),
    (0x3006, "SBGGR10_2X8_PADLO_LE"),
    (0x3007, "SBGGR10_1X10"),
    (0x3008, "SBGGR12_1X12"),
    (0x3009, "SGRBG10_DPCM8_1X8"),
    (0x300a, "SGRBG10_1X10"),
    (0x300b, "SBGGR10_DPCM8_1X8"),
    (0x300c, "SGBRG10_DPCM8_1X8"),
    (0x300d, "SRGGB10_DPCM8_1X8"),
    (0x300e, "SGBRG10_1X10"),
    (0x300f, "SRGGB10_1X10"),
    (0x3010, "SGBRG12_1X12"),
    (0x3011, "SGRBG12_1X12"),
    (0x3012, "SRGGB12_1X12"),
    (0x3013, "SGBRG8_1X8"),
    (0x3014, "SRGGB8_1X8"),
    (0x3015, "SBGGR10_ALAW8_1X8"),
    (0x3016, "SGBRG10_ALAW8_1X8"),
    (0x3017, "SGRBG10_ALAW8_1X8"),
    (0x3018, "SRGGB10_ALAW8_1X8"),
    (0x3019, "SBGGR14_1X14"),
    (0x301a, "SGBRG14_1X14"),
    (0x301b, "SGRBG14_1X14"),
    (0x301c, "SRGGB14_1X14"),
    (0x301d, "SBGGR16_1X16"),
    (0x301e, "SGBRG16_1X16"),
    (0x301f, "SGRBG16_1X16"),
    (0x3020, "SRGGB16_1X16"),
    (0x3021, "SBGGR20_1X20"),
    (0x3022, "SGBRG20_1X20"),
    (0x3023, "SGRBG20_1X20"),
    (0x3024, "SRGGB20_1X20"),
    (0x4001, "JPEG_1X8"),
    (0x5001, "S5C_UYVY_JPEG_1X8"),
    (0x6001, "AHSV8888_1X32"),
    (0x7001, "METADATA_FIXED"),
    (0x8001, "META_8"),
    (0x8002, "META_10"),
    (0x8003, "META_12"),
    (0x8004, "META_14"),
    (0x8005, "META_16"),
    (0x8006, "META_20"),
    (0x8007, "META_24"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sorted_and_found() {
        assert!(NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(MbusCode::SRGGB10_1X10.name(), Some("SRGGB10_1X10"));
        assert_eq!(
            MbusCode::from_name("MEDIA_BUS_FMT_Y10_1X10"),
            Some(MbusCode::Y10_1X10)
        );
        assert_eq!(format!("{:?}", MbusCode(0x9999)), "0x9999");
    }
}

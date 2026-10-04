//! Media bus codes (`MEDIA_BUS_FMT_*`) and colour filter arrangements.

use alloc::format;
use core::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Colour filter arrangement of the pixel array, as seen with no flips applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum ColorFilter {
    /// Bayer, first row B G.
    #[serde(rename = "BGGR")]
    Bggr,
    /// Bayer, first row G B.
    #[serde(rename = "GBRG")]
    Gbrg,
    /// Bayer, first row G R.
    #[serde(rename = "GRBG")]
    Grbg,
    /// Bayer, first row R G.
    #[serde(rename = "RGGB")]
    Rggb,
    /// No colour filter.
    #[serde(rename = "mono")]
    Mono,
}

impl ColorFilter {
    /// The order after flipping (`hflip` swaps columns, `vflip` swaps rows).
    pub fn flipped(self, hflip: bool, vflip: bool) -> Self {
        use ColorFilter::*;
        let h = |c| match c {
            Bggr => Gbrg,
            Gbrg => Bggr,
            Grbg => Rggb,
            Rggb => Grbg,
            Mono => Mono,
        };
        let v = |c| match c {
            Bggr => Grbg,
            Grbg => Bggr,
            Gbrg => Rggb,
            Rggb => Gbrg,
            Mono => Mono,
        };
        let c = if hflip { h(self) } else { self };
        if vflip { v(c) } else { c }
    }
}

/// A media bus code (`MEDIA_BUS_FMT_*`). In TOML either the name without the prefix
/// (`"SBGGR10_1X10"`) or the number (`0x3007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MbusCode(pub u32);

const CODES: &[(&str, u32, Option<ColorFilter>, u8)] = &[
    ("Y8_1X8", 0x2001, Some(ColorFilter::Mono), 8),
    ("Y10_1X10", 0x200a, Some(ColorFilter::Mono), 10),
    ("Y12_1X12", 0x2013, Some(ColorFilter::Mono), 12),
    ("Y14_1X14", 0x202d, Some(ColorFilter::Mono), 14),
    ("Y16_1X16", 0x202e, Some(ColorFilter::Mono), 16),
    ("SBGGR8_1X8", 0x3001, Some(ColorFilter::Bggr), 8),
    ("SGBRG8_1X8", 0x3013, Some(ColorFilter::Gbrg), 8),
    ("SGRBG8_1X8", 0x3002, Some(ColorFilter::Grbg), 8),
    ("SRGGB8_1X8", 0x3014, Some(ColorFilter::Rggb), 8),
    ("SBGGR10_1X10", 0x3007, Some(ColorFilter::Bggr), 10),
    ("SGBRG10_1X10", 0x300e, Some(ColorFilter::Gbrg), 10),
    ("SGRBG10_1X10", 0x300a, Some(ColorFilter::Grbg), 10),
    ("SRGGB10_1X10", 0x300f, Some(ColorFilter::Rggb), 10),
    ("SBGGR12_1X12", 0x3008, Some(ColorFilter::Bggr), 12),
    ("SGBRG12_1X12", 0x3010, Some(ColorFilter::Gbrg), 12),
    ("SGRBG12_1X12", 0x3011, Some(ColorFilter::Grbg), 12),
    ("SRGGB12_1X12", 0x3012, Some(ColorFilter::Rggb), 12),
    ("SBGGR14_1X14", 0x3019, Some(ColorFilter::Bggr), 14),
    ("SGBRG14_1X14", 0x301a, Some(ColorFilter::Gbrg), 14),
    ("SGRBG14_1X14", 0x301b, Some(ColorFilter::Grbg), 14),
    ("SRGGB14_1X14", 0x301c, Some(ColorFilter::Rggb), 14),
    ("SBGGR16_1X16", 0x301d, Some(ColorFilter::Bggr), 16),
    ("SGBRG16_1X16", 0x301e, Some(ColorFilter::Gbrg), 16),
    ("SGRBG16_1X16", 0x301f, Some(ColorFilter::Grbg), 16),
    ("SRGGB16_1X16", 0x3020, Some(ColorFilter::Rggb), 16),
];

impl MbusCode {
    /// `MEDIA_BUS_FMT_SBGGR10_1X10`.
    pub const SBGGR10_1X10: MbusCode = MbusCode(0x3007);
    /// `MEDIA_BUS_FMT_SBGGR8_1X8`.
    pub const SBGGR8_1X8: MbusCode = MbusCode(0x3001);

    /// Look a code up by name (without `MEDIA_BUS_FMT_`).
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.strip_prefix("MEDIA_BUS_FMT_").unwrap_or(name);
        CODES.iter().find(|c| c.0 == name).map(|c| MbusCode(c.1))
    }

    /// The name, if this crate knows the code.
    pub fn name(self) -> Option<&'static str> {
        CODES.iter().find(|c| c.1 == self.0).map(|c| c.0)
    }

    /// Bits per pixel, if known.
    pub fn bit_depth(self) -> Option<u8> {
        CODES.iter().find(|c| c.1 == self.0).map(|c| c.3)
    }

    /// Colour filter order the code implies, if known.
    pub fn color_filter(self) -> Option<ColorFilter> {
        CODES.iter().find(|c| c.1 == self.0).and_then(|c| c.2)
    }

    /// The code of the same bit depth with colour filter order `cf` (`None` for codes this
    /// crate does not know, or no such code).
    pub fn with_color_filter(self, cf: ColorFilter) -> Option<Self> {
        let bits = self.bit_depth()?;
        CODES
            .iter()
            .find(|c| c.3 == bits && c.2 == Some(cf))
            .map(|c| MbusCode(c.1))
    }

    /// The code the sensor sends after flipping (`hflip` swaps columns, `vflip` rows). Codes
    /// this crate does not know, and mono codes, stay as they are. Flipping twice undoes it.
    pub fn flipped(self, hflip: bool, vflip: bool) -> Self {
        self.color_filter()
            .and_then(|cf| self.with_color_filter(cf.flipped(hflip, vflip)))
            .unwrap_or(self)
    }
}

impl fmt::Display for MbusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "0x{:04x}", self.0),
        }
    }
}

impl<'de> Deserialize<'de> for MbusCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = MbusCode;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a media bus code name such as \"SBGGR10_1X10\" or a number")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<MbusCode, E> {
                MbusCode::from_name(v)
                    .ok_or_else(|| E::custom(format!("unknown media bus code '{v}'")))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<MbusCode, E> {
                u32::try_from(v)
                    .map(MbusCode)
                    .map_err(|_| E::custom("media bus code out of range"))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers() {
        assert_eq!(MbusCode::from_name("SBGGR10_1X10"), Some(MbusCode(0x3007)));
        assert_eq!(
            MbusCode::from_name("MEDIA_BUS_FMT_Y8_1X8"),
            Some(MbusCode(0x2001))
        );
        assert_eq!(MbusCode(0x3001).bit_depth(), Some(8));
        assert_eq!(MbusCode(0x3001).to_string(), "SBGGR8_1X8");
        assert_eq!(MbusCode(0x1234).to_string(), "0x1234");
    }

    #[test]
    fn flips_change_bayer_order() {
        use ColorFilter::*;
        assert_eq!(Bggr.flipped(true, false), Gbrg);
        assert_eq!(Bggr.flipped(false, true), Grbg);
        assert_eq!(Bggr.flipped(true, true), Rggb);
        assert_eq!(Mono.flipped(true, true), Mono);
        let c = MbusCode(0x300f); // SRGGB10
        assert_eq!(c.flipped(true, true), MbusCode(0x3007));
        assert_eq!(c.flipped(true, false).flipped(true, false), c);
        assert_eq!(MbusCode(0x3001).flipped(false, true), MbusCode(0x3002));
        assert_eq!(MbusCode(0x200a).flipped(true, true), MbusCode(0x200a));
        assert_eq!(MbusCode(0x1234).flipped(true, true), MbusCode(0x1234));
    }
}

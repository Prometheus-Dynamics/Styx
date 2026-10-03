//! Four-character codes identifying V4L2 pixel and metadata formats.

use std::fmt;
use std::str::FromStr;

/// A V4L2 four-character code (`v4l2_fourcc(a, b, c, d)`), e.g. `YUYV`, `MJPG`, `pRAA`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FourCc(pub u32);

impl FourCc {
    /// Bit 31 marks the big-endian variant of a format (`v4l2_fourcc_be`).
    pub const BIG_ENDIAN: u32 = 1 << 31;

    /// Builds a code from its four characters.
    pub const fn new(code: &[u8; 4]) -> Self {
        Self(u32::from_le_bytes(*code))
    }

    /// The raw value.
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// The four characters (without the big-endian flag).
    pub const fn to_bytes(self) -> [u8; 4] {
        (self.0 & !Self::BIG_ENDIAN).to_le_bytes()
    }

    /// True for the big-endian variant of a format.
    pub const fn is_big_endian(self) -> bool {
        self.0 & Self::BIG_ENDIAN != 0
    }

    /// Packed YUV 4:2:2 (`YUYV`).
    pub const YUYV: Self = Self::new(b"YUYV");
    /// Packed YUV 4:2:2 (`UYVY`).
    pub const UYVY: Self = Self::new(b"UYVY");
    /// Motion-JPEG (`MJPG`).
    pub const MJPG: Self = Self::new(b"MJPG");
    /// JPEG (`JPEG`).
    pub const JPEG: Self = Self::new(b"JPEG");
    /// H.264 (`H264`).
    pub const H264: Self = Self::new(b"H264");
    /// Y/CbCr 4:2:0, two planes in one buffer (`NV12`).
    pub const NV12: Self = Self::new(b"NV12");
    /// Y/CbCr 4:2:0, three planes in one buffer (`YU12`).
    pub const YUV420: Self = Self::new(b"YU12");
    /// 24-bit RGB (`RGB3`).
    pub const RGB24: Self = Self::new(b"RGB3");
    /// 24-bit BGR (`BGR3`).
    pub const BGR24: Self = Self::new(b"BGR3");
    /// 8-bit greyscale (`GREY`).
    pub const GREY: Self = Self::new(b"GREY");
    /// 10-bit greyscale in 16 bits (`Y10 `).
    pub const Y10: Self = Self::new(b"Y10 ");
    /// 10-bit greyscale, MIPI CSI-2 packed (`Y10P`).
    pub const Y10P: Self = Self::new(b"Y10P");
    /// 8-bit Bayer BGGR (`BA81`).
    pub const SBGGR8: Self = Self::new(b"BA81");
    /// 10-bit Bayer RGGB, CSI-2 packed (`pRAA`).
    pub const SRGGB10P: Self = Self::new(b"pRAA");
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.to_bytes() {
            let c = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            };
            write!(f, "{c}")?;
        }
        if self.is_big_endian() {
            f.write_str("-BE")?;
        }
        Ok(())
    }
}

impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCc({self})")
    }
}

impl From<u32> for FourCc {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<FourCc> for u32 {
    fn from(v: FourCc) -> Self {
        v.0
    }
}

impl FromStr for FourCc {
    type Err = crate::Error;

    /// Parses up to four characters (padded with spaces), optionally followed by `-BE`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (body, be) = match s.strip_suffix("-BE") {
            Some(body) => (body, true),
            None => (s, false),
        };
        let bytes = body.as_bytes();
        if bytes.is_empty() || bytes.len() > 4 || !bytes.iter().all(u8::is_ascii) {
            return Err(crate::Error::Invalid(format!("not a fourcc: {s:?}")));
        }
        let mut code = [b' '; 4];
        code[..bytes.len()].copy_from_slice(bytes);
        let mut v = Self::new(&code).0;
        if be {
            v |= Self::BIG_ENDIAN;
        }
        Ok(Self(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_v4l2_fourcc_macro() {
        // v4l2_fourcc('Y','U','Y','V') = 'Y' | 'U'<<8 | 'Y'<<16 | 'V'<<24
        assert_eq!(FourCc::YUYV.0, 0x5659_5559);
        assert_eq!(FourCc::MJPG.0, 0x4750_4a4d);
    }

    #[test]
    fn display_and_parse_round_trip() {
        assert_eq!(FourCc::YUYV.to_string(), "YUYV");
        assert_eq!(FourCc::Y10.to_string(), "Y10 ");
        assert_eq!("Y10".parse::<FourCc>().unwrap(), FourCc::Y10);
        assert_eq!("pRAA".parse::<FourCc>().unwrap(), FourCc::SRGGB10P);
        let be: FourCc = "Y16 -BE".parse().unwrap();
        assert!(be.is_big_endian());
        assert_eq!(be.to_string(), "Y16 -BE");
        assert!("TOOLONG".parse::<FourCc>().is_err());
        assert_eq!(FourCc(0x0000_0001).to_string(), "....");
    }
}

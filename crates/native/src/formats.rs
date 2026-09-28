//! Raw formats: which pixel formats a CSI-2 receiver writes for a media bus code, and how many
//! bytes a line takes.

use styx_kernel::FourCc;

/// How the samples of a raw format are laid out in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawPacking {
    /// One byte per sample.
    Bits8,
    /// MIPI CSI-2 packed 10-bit: four samples in five bytes.
    Csi2Packed10,
    /// MIPI CSI-2 packed 12-bit: two samples in three bytes.
    Csi2Packed12,
    /// Samples in the low bits of 16-bit little-endian words.
    Unpacked16,
}

impl RawPacking {
    /// Bytes for `width` samples, without padding.
    pub fn line_bytes(self, width: u32) -> u32 {
        match self {
            RawPacking::Bits8 => width,
            RawPacking::Csi2Packed10 => width.div_ceil(4) * 5,
            RawPacking::Csi2Packed12 => width.div_ceil(2) * 3,
            RawPacking::Unpacked16 => width * 2,
        }
    }
}

/// A memory format a receiver can write for a bus code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawFormat {
    /// The V4L2 pixel format.
    pub fourcc: FourCc,
    /// Its layout.
    pub packing: RawPacking,
    /// Bits per sample.
    pub bits: u8,
}

const fn raw(code: &[u8; 4], packing: RawPacking, bits: u8) -> RawFormat {
    RawFormat {
        fourcc: FourCc::new(code),
        packing,
        bits,
    }
}

use RawPacking::*;

/// `(bus code, formats)`, CSI-2 packed first.
static TABLE: [(u32, &[RawFormat]); 15] = [
    // MEDIA_BUS_FMT_S{BGGR,GBRG,GRBG,RGGB}8_1X8
    (0x3001, &[raw(b"BA81", Bits8, 8)]),
    (0x3013, &[raw(b"GBRG", Bits8, 8)]),
    (0x3002, &[raw(b"GRBG", Bits8, 8)]),
    (0x3014, &[raw(b"RGGB", Bits8, 8)]),
    // ..10_1X10
    (
        0x3007,
        &[raw(b"pBAA", Csi2Packed10, 10), raw(b"BG10", Unpacked16, 10)],
    ),
    (
        0x300e,
        &[raw(b"pGAA", Csi2Packed10, 10), raw(b"GB10", Unpacked16, 10)],
    ),
    (
        0x300a,
        &[raw(b"pgAA", Csi2Packed10, 10), raw(b"BA10", Unpacked16, 10)],
    ),
    (
        0x300f,
        &[raw(b"pRAA", Csi2Packed10, 10), raw(b"RG10", Unpacked16, 10)],
    ),
    // ..12_1X12
    (
        0x3008,
        &[raw(b"pBCC", Csi2Packed12, 12), raw(b"BG12", Unpacked16, 12)],
    ),
    (
        0x3010,
        &[raw(b"pGCC", Csi2Packed12, 12), raw(b"GB12", Unpacked16, 12)],
    ),
    (
        0x3011,
        &[raw(b"pgCC", Csi2Packed12, 12), raw(b"BA12", Unpacked16, 12)],
    ),
    (
        0x3012,
        &[raw(b"pRCC", Csi2Packed12, 12), raw(b"RG12", Unpacked16, 12)],
    ),
    // MEDIA_BUS_FMT_Y8_1X8, Y10_1X10, Y12_1X12
    (0x2001, &[raw(b"GREY", Bits8, 8)]),
    (
        0x200a,
        &[raw(b"Y10P", Csi2Packed10, 10), raw(b"Y10 ", Unpacked16, 10)],
    ),
    (
        0x2013,
        &[raw(b"Y12P", Csi2Packed12, 12), raw(b"Y12 ", Unpacked16, 12)],
    ),
];

/// The memory formats for a media bus code, CSI-2 packed first (what `rp1-cfe` and most CSI-2
/// receivers write without conversion). Empty for codes this table does not know.
pub fn memory_formats(code: u32) -> &'static [RawFormat] {
    TABLE
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(&[], |(_, f)| f)
}

/// The table entry for a pixel format, whatever the bus code.
pub fn raw_format(fourcc: FourCc) -> Option<RawFormat> {
    TABLE
        .iter()
        .flat_map(|(_, f)| f.iter())
        .find(|f| f.fourcc == fourcc)
        .copied()
}

/// Picks the format to capture in: the first of the table's formats for `code` that the node
/// offers (`offered` empty means "not known": the table's first), else the node's first.
pub fn choose(code: u32, offered: &[FourCc]) -> Option<FourCc> {
    let table = memory_formats(code);
    if offered.is_empty() {
        return table.first().map(|f| f.fourcc);
    }
    table
        .iter()
        .map(|f| f.fourcc)
        .find(|f| offered.contains(f))
        .or_else(|| offered.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_formats_come_first_and_lines_have_the_right_size() {
        let f = memory_formats(0x3007);
        assert_eq!(f[0].fourcc, FourCc::new(b"pBAA"));
        assert_eq!(f[0].packing.line_bytes(1280), 1600);
        assert_eq!(f[1].packing.line_bytes(1280), 2560);
        assert_eq!(RawPacking::Csi2Packed12.line_bytes(640), 960);
        assert_eq!(RawPacking::Bits8.line_bytes(640), 640);
        assert!(memory_formats(0x1234).is_empty());
    }

    #[test]
    fn looks_formats_up_by_fourcc() {
        let f = raw_format(FourCc::new(b"pRAA")).unwrap();
        assert_eq!((f.bits, f.packing), (10, RawPacking::Csi2Packed10));
        assert!(raw_format(FourCc::new(b"NV12")).is_none());
    }

    #[test]
    fn chooses_what_the_node_offers() {
        let pbaa = FourCc::new(b"pBAA");
        let bg10 = FourCc::new(b"BG10");
        assert_eq!(choose(0x3007, &[]), Some(pbaa));
        assert_eq!(choose(0x3007, &[bg10, pbaa]), Some(pbaa));
        assert_eq!(choose(0x3007, &[bg10]), Some(bg10));
        let odd = FourCc::new(b"XXXX");
        assert_eq!(choose(0x3007, &[odd]), Some(odd));
        assert_eq!(choose(0x1234, &[]), None);
    }
}

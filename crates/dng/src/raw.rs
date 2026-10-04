//! Raw frames: samples, colour filter layout, unpacking of sensor packings.

use alloc::{format, vec, vec::Vec};

use crate::{Result, invalid};

/// A 2x2 Bayer colour filter pattern, named by its first row then its second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CfaPattern {
    /// R G / G B.
    Rggb,
    /// B G / G R.
    Bggr,
    /// G R / B G.
    Grbg,
    /// G B / R G.
    Gbrg,
}

impl CfaPattern {
    /// DNG `CFAPattern` colours of the 2x2 cell in row-major order (0 red, 1 green, 2 blue).
    pub fn colors(self) -> [u8; 4] {
        match self {
            Self::Rggb => [0, 1, 1, 2],
            Self::Bggr => [2, 1, 1, 0],
            Self::Grbg => [1, 0, 2, 1],
            Self::Gbrg => [1, 2, 0, 1],
        }
    }

    /// The pattern with these `CFAPattern` colours, if it is a Bayer pattern.
    pub fn from_colors(c: &[u8]) -> Option<Self> {
        [Self::Rggb, Self::Bggr, Self::Grbg, Self::Gbrg]
            .into_iter()
            .find(|p| p.colors() == c)
    }

    /// Colour (0 red, 1 green, 2 blue) of pixel (`x`, `y`).
    pub fn color_at(self, x: usize, y: usize) -> u8 {
        self.colors()[(y & 1) * 2 + (x & 1)]
    }

    /// The `(row, column)` of each colour's first sample in the 2x2 cell: red, the green on
    /// red's row, the green on blue's row, blue.
    pub fn offsets(self) -> [(u32, u32); 4] {
        let c = self.colors();
        let at = |i: usize| ((i / 2) as u32, (i % 2) as u32);
        let r = c.iter().position(|&v| v == 0).unwrap_or(0);
        let b = c.iter().position(|&v| v == 2).unwrap_or(3);
        // The green on red's row: same row as red, the other column.
        let gr = (r & 2) | (1 - (r & 1));
        let gb = (b & 2) | (1 - (b & 1));
        [at(r), at(gr), at(gb), at(b)]
    }
}

/// How the samples are arranged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleLayout {
    /// One colour per pixel through a 2x2 Bayer filter.
    Cfa(CfaPattern),
    /// Monochrome (no colour filter).
    Mono,
}

/// How samples are stored in the rows handed to [`unpack`] / [`RawImage::from_packed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Packing {
    /// One byte per sample.
    U8,
    /// Two bytes per sample, little-endian, `bits` significant bits (right-aligned).
    U16Le {
        /// Significant bits.
        bits: u8,
    },
    /// MIPI CSI-2 RAW10: four samples in five bytes (four high bytes, then the low bits).
    Csi2Raw10,
    /// MIPI CSI-2 RAW12: two samples in three bytes.
    Csi2Raw12,
    /// MIPI CSI-2 RAW14: four samples in seven bytes.
    Csi2Raw14,
}

impl Packing {
    /// Significant bits per sample.
    pub fn bits(self) -> u8 {
        match self {
            Self::U8 => 8,
            Self::U16Le { bits } => bits,
            Self::Csi2Raw10 => 10,
            Self::Csi2Raw12 => 12,
            Self::Csi2Raw14 => 14,
        }
    }

    /// Bytes holding `width` samples.
    pub fn row_bytes(self, width: usize) -> usize {
        match self {
            Self::U8 => width,
            Self::U16Le { .. } => width * 2,
            Self::Csi2Raw10 => width.div_ceil(4) * 5,
            Self::Csi2Raw12 => width.div_ceil(2) * 3,
            Self::Csi2Raw14 => width.div_ceil(4) * 7,
        }
    }
}

/// Unpacks one row of `width` samples into `out`.
fn unpack_row(packing: Packing, row: &[u8], out: &mut [u16]) {
    match packing {
        Packing::U8 => {
            for (o, &b) in out.iter_mut().zip(row) {
                *o = u16::from(b);
            }
        }
        Packing::U16Le { bits } => {
            let mask = if bits >= 16 {
                u16::MAX
            } else {
                (1u16 << bits) - 1
            };
            for (o, c) in out.iter_mut().zip(row.chunks_exact(2)) {
                *o = u16::from_le_bytes([c[0], c[1]]) & mask;
            }
        }
        Packing::Csi2Raw10 => {
            for (o, c) in out.chunks_mut(4).zip(row.chunks(5)) {
                let low = c.get(4).copied().unwrap_or(0);
                for (i, v) in o.iter_mut().enumerate() {
                    *v = (u16::from(c[i]) << 2) | u16::from((low >> (2 * i)) & 3);
                }
            }
        }
        Packing::Csi2Raw12 => {
            for (o, c) in out.chunks_mut(2).zip(row.chunks(3)) {
                let low = c.get(2).copied().unwrap_or(0);
                for (i, v) in o.iter_mut().enumerate() {
                    *v = (u16::from(c[i]) << 4) | u16::from((low >> (4 * i)) & 15);
                }
            }
        }
        Packing::Csi2Raw14 => {
            for (o, c) in out.chunks_mut(4).zip(row.chunks(7)) {
                let mut low = 0u32;
                for (i, &b) in c.iter().skip(4).enumerate() {
                    low |= u32::from(b) << (8 * i);
                }
                for (i, v) in o.iter_mut().enumerate() {
                    *v = (u16::from(c[i]) << 6) | ((low >> (6 * i)) & 63) as u16;
                }
            }
        }
    }
}

/// Unpacks a frame of `width` x `height` samples whose rows start `stride` bytes apart.
pub fn unpack(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    packing: Packing,
) -> Result<Vec<u16>> {
    let (w, h) = (width as usize, height as usize);
    let row = packing.row_bytes(w);
    if stride < row {
        return Err(invalid(format!("stride {stride} < {row} bytes per row")));
    }
    if h > 0 && data.len() < stride * (h - 1) + row {
        return Err(invalid(format!(
            "{} bytes for {h} rows of {stride}",
            data.len()
        )));
    }
    let mut out = vec![0u16; w * h];
    for (y, o) in out.chunks_mut(w.max(1)).enumerate().take(h) {
        unpack_row(packing, &data[y * stride..y * stride + row], o);
    }
    Ok(out)
}

/// A raw frame: one sample per pixel, right-aligned in `bits` bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Colour filter (or none).
    pub layout: SampleLayout,
    /// Significant bits per sample (8 to 16).
    pub bits: u8,
    /// Samples, row-major, `width * height`.
    pub samples: Vec<u16>,
}

impl RawImage {
    /// A frame from samples.
    pub fn new(
        width: u32,
        height: u32,
        layout: SampleLayout,
        bits: u8,
        samples: Vec<u16>,
    ) -> Result<Self> {
        if !(1..=16).contains(&bits) {
            return Err(invalid(format!("{bits} bits per sample")));
        }
        if samples.len() != width as usize * height as usize || width == 0 || height == 0 {
            return Err(invalid(format!(
                "{} samples for {width}x{height}",
                samples.len()
            )));
        }
        if matches!(layout, SampleLayout::Cfa(_))
            && (!width.is_multiple_of(2) || !height.is_multiple_of(2))
        {
            return Err(invalid("a Bayer frame needs even width and height"));
        }
        Ok(Self {
            width,
            height,
            layout,
            bits,
            samples,
        })
    }

    /// A frame from rows stored with `packing`, `stride` bytes apart.
    pub fn from_packed(
        data: &[u8],
        width: u32,
        height: u32,
        stride: usize,
        packing: Packing,
        layout: SampleLayout,
    ) -> Result<Self> {
        let samples = unpack(data, width, height, stride, packing)?;
        Self::new(width, height, layout, packing.bits(), samples)
    }

    /// The largest sample value the bit depth allows.
    pub fn max_value(&self) -> u32 {
        (1u32 << self.bits) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi2_packings_unpack() {
        // RAW10: samples 0x3ff, 0x001, 0x200, 0x155.
        let row = [0xff, 0x00, 0x80, 0x55, 0b01_00_01_11];
        let mut out = [0u16; 4];
        unpack_row(Packing::Csi2Raw10, &row, &mut out);
        assert_eq!(out, [0x3ff, 0x001, 0x200, 0x155]);
        // RAW12: 0xabc, 0x123.
        let mut out = [0u16; 2];
        unpack_row(Packing::Csi2Raw12, &[0xab, 0x12, 0x3c], &mut out);
        assert_eq!(out, [0xabc, 0x123]);
        // RAW14: four samples, low bits packed 6 per sample after four high bytes.
        let vals = [0x3fffu16, 0x0001, 0x2000, 0x1555];
        let mut row = [0u8; 7];
        let mut low = 0u32;
        for (i, v) in vals.iter().enumerate() {
            row[i] = (v >> 6) as u8;
            low |= u32::from(v & 63) << (6 * i);
        }
        row[4..].copy_from_slice(&low.to_le_bytes()[..3]);
        let mut out = [0u16; 4];
        unpack_row(Packing::Csi2Raw14, &row, &mut out);
        assert_eq!(out, vals);
        let le = unpack(
            &[0x34, 0x12, 0xff, 0xff],
            2,
            1,
            4,
            Packing::U16Le { bits: 12 },
        )
        .unwrap();
        assert_eq!(le, [0x234, 0xfff]);
    }

    #[test]
    fn frames_check_their_sizes() {
        let data = vec![0u8; 5 * 2 + 3];
        // Stride 6 (padding), two rows of 4 RAW10 samples.
        let img = RawImage::from_packed(
            &data,
            4,
            2,
            6,
            Packing::Csi2Raw10,
            SampleLayout::Cfa(CfaPattern::Bggr),
        )
        .unwrap();
        assert_eq!((img.samples.len(), img.max_value()), (8, 1023));
        assert!(unpack(&data, 4, 3, 6, Packing::Csi2Raw10).is_err());
        assert!(RawImage::new(3, 2, SampleLayout::Cfa(CfaPattern::Rggb), 10, vec![0; 6]).is_err());
        assert!(RawImage::new(3, 2, SampleLayout::Mono, 10, vec![0; 6]).is_ok());
    }

    #[test]
    fn bayer_offsets() {
        assert_eq!(CfaPattern::Bggr.offsets(), [(1, 1), (1, 0), (0, 1), (0, 0)]);
        assert_eq!(CfaPattern::Grbg.offsets(), [(0, 1), (0, 0), (1, 1), (1, 0)]);
        for p in [
            CfaPattern::Rggb,
            CfaPattern::Bggr,
            CfaPattern::Grbg,
            CfaPattern::Gbrg,
        ] {
            assert_eq!(CfaPattern::from_colors(&p.colors()), Some(p));
            let [r, gr, gb, b] = p.offsets();
            assert_eq!(p.color_at(r.1 as usize, r.0 as usize), 0);
            assert_eq!(p.color_at(b.1 as usize, b.0 as usize), 2);
            assert_eq!(p.color_at(gr.1 as usize, gr.0 as usize), 1);
            assert_eq!(p.color_at(gb.1 as usize, gb.0 as usize), 1);
            assert_eq!(gr.0, r.0);
            assert_eq!(gb.0, b.0);
        }
    }
}

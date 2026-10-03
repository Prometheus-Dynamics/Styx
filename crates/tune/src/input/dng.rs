//! A minimal DNG reader: uncompressed CFA images (8 or 16 bits per sample, or packed 10/12/14
//! bits), in the first IFD or a sub-IFD, with `CFAPattern`, `BlackLevel`, `WhiteLevel`,
//! `ExposureTime` and `ISOSpeedRatings` (gain = ISO / 100, as `picamera2` writes it).
//! Compressed (lossless JPEG) DNGs are refused; plug a full decoder in with
//! [`super::Loader::with_dng`] for those.

use super::RawDecoder;
use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// The built-in reader.
#[derive(Clone, Copy, Debug, Default)]
pub struct MinimalDng;

impl RawDecoder for MinimalDng {
    fn decode(&self, bytes: &[u8]) -> Result<Vec<RawFrame>> {
        decode(bytes).map(|f| vec![f])
    }
}

fn bad(m: impl Into<String>) -> TuneError {
    TuneError::Format(format!("DNG: {}", m.into()))
}

struct Tiff<'a> {
    b: &'a [u8],
    le: bool,
}

#[derive(Clone, Debug)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Offset of the value (inline in the entry when it fits in 4 bytes).
    at: usize,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Result<u16> {
        let s = self.b.get(at..at + 2).ok_or_else(|| bad("truncated"))?;
        Ok(if self.le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    }

    fn u32(&self, at: usize) -> Result<u32> {
        let s = self.b.get(at..at + 4).ok_or_else(|| bad("truncated"))?;
        let a = [s[0], s[1], s[2], s[3]];
        Ok(if self.le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }

    fn ifd(&self, at: usize) -> Result<Vec<Entry>> {
        let n = self.u16(at)? as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let e = at + 2 + i * 12;
            let (tag, typ, count) = (self.u16(e)?, self.u16(e + 2)?, self.u32(e + 4)?);
            let size = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => 0,
            } * count as usize;
            let at = if size <= 4 {
                e + 8
            } else {
                self.u32(e + 8)? as usize
            };
            out.push(Entry {
                tag,
                typ,
                count,
                at,
            });
        }
        Ok(out)
    }

    /// Value `i` of an entry as a number.
    fn value(&self, e: &Entry, i: usize) -> Result<f64> {
        if i >= e.count as usize {
            return Err(bad(format!("tag {} has {} values", e.tag, e.count)));
        }
        Ok(match e.typ {
            1 | 7 => f64::from(*self.b.get(e.at + i).ok_or_else(|| bad("truncated"))?),
            3 => f64::from(self.u16(e.at + 2 * i)?),
            4 | 13 => f64::from(self.u32(e.at + 4 * i)?),
            9 => f64::from(self.u32(e.at + 4 * i)? as i32),
            5 => {
                let (n, d) = (self.u32(e.at + 8 * i)?, self.u32(e.at + 8 * i + 4)?);
                f64::from(n) / f64::from(d.max(1))
            }
            10 => {
                let (n, d) = (
                    self.u32(e.at + 8 * i)? as i32,
                    self.u32(e.at + 8 * i + 4)? as i32,
                );
                f64::from(n) / f64::from(if d == 0 { 1 } else { d })
            }
            11 => f64::from(f32::from_bits(self.u32(e.at + 4 * i)?)),
            _ => {
                return Err(bad(format!(
                    "tag {} has an unsupported type {}",
                    e.tag, e.typ
                )));
            }
        })
    }

    fn values(&self, e: &Entry) -> Result<Vec<f64>> {
        (0..e.count as usize).map(|i| self.value(e, i)).collect()
    }
}

fn find(ifd: &[Entry], tag: u16) -> Option<&Entry> {
    ifd.iter().find(|e| e.tag == tag)
}

const SUBFILE: u16 = 254;
const WIDTH: u16 = 256;
const HEIGHT: u16 = 257;
const BITS: u16 = 258;
const COMPRESSION: u16 = 259;
const PHOTOMETRIC: u16 = 262;
const STRIP_OFFSETS: u16 = 273;
const STRIP_BYTES: u16 = 279;
const SUB_IFDS: u16 = 330;
const CFA_PATTERN: u16 = 33422;
const EXPOSURE: u16 = 33434;
const ISO: u16 = 34855;
const EXIF: u16 = 34665;
const BLACK: u16 = 50714;
const WHITE: u16 = 50717;
const CFA: u32 = 32803;

/// Decode the raw image of a DNG.
pub fn decode(b: &[u8]) -> Result<RawFrame> {
    let le = match b.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return Err(bad("not a TIFF/DNG file")),
    };
    let t = Tiff { b, le };
    let ifd0 = t.ifd(t.u32(4)? as usize)?;
    // The raw image: IFD 0 or a sub-IFD with CFA photometric interpretation.
    let mut candidates = vec![ifd0.clone()];
    if let Some(e) = find(&ifd0, SUB_IFDS) {
        for off in t.values(e)? {
            candidates.push(t.ifd(off as usize)?);
        }
    }
    let raw = candidates
        .iter()
        .find(|ifd| {
            find(ifd, PHOTOMETRIC).is_some_and(|e| t.value(e, 0).ok() == Some(f64::from(CFA)))
                && find(ifd, SUBFILE).is_none_or(|e| t.value(e, 0).ok() == Some(0.0))
        })
        .ok_or_else(|| bad("no CFA image"))?;
    let num = |ifd: &[Entry], tag| -> Result<Option<f64>> {
        find(ifd, tag).map(|e| t.value(e, 0)).transpose()
    };
    let need = |tag, name: &str| -> Result<f64> {
        num(raw, tag)?.ok_or_else(|| bad(format!("{name} missing")))
    };
    let (w, h) = (
        need(WIDTH, "ImageWidth")? as usize,
        need(HEIGHT, "ImageLength")? as usize,
    );
    let stored_bits = need(BITS, "BitsPerSample")? as u32;
    if num(raw, COMPRESSION)?.unwrap_or(1.0) != 1.0 {
        return Err(bad(
            "compressed DNG; only uncompressed images are read here",
        ));
    }
    if !matches!(stored_bits, 8 | 10 | 12 | 14 | 16) {
        return Err(bad(format!("{stored_bits} bits per sample")));
    }
    let cfa = match find(raw, CFA_PATTERN) {
        Some(e) => {
            let v = t.values(e)?;
            let code: Vec<u8> = v.iter().take(4).map(|&x| x as u8).collect();
            match code.as_slice() {
                [0, 1, 1, 2] => Cfa::Rggb,
                [1, 0, 2, 1] => Cfa::Grbg,
                [1, 2, 0, 1] => Cfa::Gbrg,
                [2, 1, 1, 0] => Cfa::Bggr,
                _ => return Err(bad(format!("CFA pattern {code:?}"))),
            }
        }
        None => return Err(bad("CFAPattern missing")),
    };
    // Strips, concatenated in order.
    let offs =
        t.values(find(raw, STRIP_OFFSETS).ok_or_else(|| bad("only strip images are read"))?)?;
    let lens = t.values(find(raw, STRIP_BYTES).ok_or_else(|| bad("StripByteCounts missing"))?)?;
    let mut bytes = Vec::new();
    for (o, l) in offs.iter().zip(&lens) {
        let s = b
            .get(*o as usize..(*o + *l) as usize)
            .ok_or_else(|| bad("strip outside the file"))?;
        bytes.extend_from_slice(s);
    }
    let data = unpack(&bytes, w, h, stored_bits, le)?;
    let white = num(raw, WHITE)?.unwrap_or(f64::from((1u32 << stored_bits) - 1));
    // Significant bits from the white level (16-bit containers of 10-bit data).
    let bits = (white + 1.0).log2().ceil().clamp(8.0, 16.0) as u8;
    let black = match find(raw, BLACK) {
        Some(e) => {
            let v = t.values(e)?;
            Some(v.iter().sum::<f64>() / v.len().max(1) as f64 / f64::from(1u32 << bits))
        }
        None => None,
    };
    // Exposure and ISO: in the raw IFD, IFD 0, or IFD 0's EXIF IFD.
    let mut places: Vec<Vec<Entry>> = vec![raw.clone(), ifd0.clone()];
    if let Some(e) = find(&ifd0, EXIF) {
        places.push(t.ifd(t.value(e, 0)? as usize)?);
    }
    let first = |tag| -> Result<Option<f64>> {
        for p in &places {
            if let Some(v) = num(p, tag)? {
                return Ok(Some(v));
            }
        }
        Ok(None)
    };
    Ok(RawFrame {
        width: w,
        height: h,
        cfa,
        bits,
        data,
        exposure_us: first(EXPOSURE)?.map_or(0.0, |s| s * 1e6),
        analogue_gain: first(ISO)?.map_or(1.0, |iso| iso / 100.0),
        digital_gain: 1.0,
        black_level: black,
    })
}

/// Samples from the strip bytes: 8 bits, 16 bits in the file's byte order, or other depths
/// packed most significant bit first, rows starting on a byte.
fn unpack(b: &[u8], w: usize, h: usize, bits: u32, le: bool) -> Result<Vec<u16>> {
    let row_bytes = (w * bits as usize).div_ceil(8);
    if b.len() < row_bytes * h {
        return Err(bad("image data is short"));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in b.chunks_exact(row_bytes).take(h) {
        match bits {
            8 => out.extend(row.iter().map(|&v| u16::from(v))),
            16 => out.extend(row.chunks_exact(2).map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })),
            _ => {
                let (mut acc, mut have, mut it) = (0u32, 0u32, row.iter());
                for _ in 0..w {
                    while have < bits {
                        acc = (acc << 8) | u32::from(*it.next().unwrap_or(&0));
                        have += 8;
                    }
                    have -= bits;
                    out.push(((acc >> have) & ((1 << bits) - 1)) as u16);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian TIFF with one IFD (test data only; Styx's DNG writer is `styx-dng`).
    fn tiff(entries: &[(u16, u16, Vec<u32>)], data: &[u8]) -> Vec<u8> {
        let mut out = b"II*\0".to_vec();
        out.extend(8u32.to_le_bytes());
        let ifd_len = 2 + entries.len() * 12 + 4;
        let mut extra: Vec<u8> = Vec::new();
        let extra_at = 8 + ifd_len;
        let mut ifd = (entries.len() as u16).to_le_bytes().to_vec();
        for (tag, typ, vals) in entries {
            ifd.extend(tag.to_le_bytes());
            ifd.extend(typ.to_le_bytes());
            let size = if *typ == 3 {
                2
            } else if *typ == 5 {
                8
            } else if *typ == 1 {
                1
            } else {
                4
            };
            let n = if *typ == 5 {
                vals.len() / 2
            } else {
                vals.len()
            };
            ifd.extend((n as u32).to_le_bytes());
            let mut bytes = Vec::new();
            for v in vals {
                match size {
                    1 => bytes.push(*v as u8),
                    2 => bytes.extend((*v as u16).to_le_bytes()),
                    _ => bytes.extend(v.to_le_bytes()),
                }
            }
            if bytes.len() <= 4 {
                bytes.resize(4, 0);
                ifd.extend(bytes);
            } else {
                ifd.extend(((extra_at + extra.len()) as u32).to_le_bytes());
                extra.extend(bytes);
            }
        }
        ifd.extend(0u32.to_le_bytes());
        out.extend(ifd);
        out.extend(extra);
        let data_at = out.len() as u32;
        out.extend(data);
        // Patch the strip offset placeholder (u32::MAX).
        let pos = out
            .windows(4)
            .position(|w| w == u32::MAX.to_le_bytes())
            .unwrap();
        out[pos..pos + 4].copy_from_slice(&data_at.to_le_bytes());
        out
    }

    #[test]
    fn reads_an_uncompressed_cfa_dng() {
        let (w, h) = (4u32, 2u32);
        let samples: Vec<u16> = (0..8).map(|i| 64 + i * 100).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let file = tiff(
            &[
                (WIDTH, 4, vec![w]),
                (HEIGHT, 4, vec![h]),
                (BITS, 3, vec![16]),
                (COMPRESSION, 3, vec![1]),
                (PHOTOMETRIC, 3, vec![CFA]),
                (STRIP_OFFSETS, 4, vec![u32::MAX]),
                (STRIP_BYTES, 4, vec![data.len() as u32]),
                (CFA_PATTERN, 1, vec![2, 1, 1, 0]),
                (EXPOSURE, 5, vec![1, 100]),
                (ISO, 3, vec![400]),
                (BLACK, 3, vec![64]),
                (WHITE, 3, vec![1023]),
            ],
            &data,
        );
        let f = decode(&file).unwrap();
        assert_eq!((f.width, f.height, f.cfa, f.bits), (4, 2, Cfa::Bggr, 10));
        assert_eq!(f.data, samples);
        assert!((f.exposure_us - 10_000.0).abs() < 1e-6);
        assert_eq!(f.analogue_gain, 4.0);
        assert_eq!(f.black_level, Some(64.0 / 1024.0));
        assert!(decode(b"not a dng").is_err());
    }

    #[test]
    fn unpacks_packed_depths() {
        // Two 12-bit samples 0xabc, 0x123 → bytes ab c1 23.
        assert_eq!(
            unpack(&[0xab, 0xc1, 0x23], 2, 1, 12, true).unwrap(),
            [0xabc, 0x123]
        );
        // Four 10-bit samples, one row padded to 5 bytes.
        let v = [1023u16, 0, 512, 3];
        let mut bits = 0u64;
        for s in v {
            bits = (bits << 10) | u64::from(s);
        }
        let bytes: Vec<u8> = (0..5).rev().map(|i| (bits >> (8 * i)) as u8).collect();
        assert_eq!(unpack(&bytes, 4, 1, 10, true).unwrap(), v);
    }
}

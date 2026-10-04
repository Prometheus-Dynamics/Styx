//! TIFF reading: either byte order, IFD entries decoded on demand, every offset and count
//! checked against the file.

use alloc::string::ToString;

use alloc::{format, string::String, vec::Vec};

use crate::tiff::kind;
use crate::{Result, malformed};

/// Byte order of a TIFF file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    Little,
    Big,
}

impl Order {
    pub fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        match self {
            Self::Little => u16::from_le_bytes(a),
            Self::Big => u16::from_be_bytes(a),
        }
    }

    pub fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            Self::Little => u32::from_le_bytes(a),
            Self::Big => u32::from_be_bytes(a),
        }
    }

    fn u64(self, b: &[u8]) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[..8]);
        match self {
            Self::Little => u64::from_le_bytes(a),
            Self::Big => u64::from_be_bytes(a),
        }
    }
}

/// One IFD entry: its value bytes (inline or from the file), still in the file's byte order.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub tag: u16,
    pub kind: u16,
    pub count: u32,
    pub data: Vec<u8>,
}

fn type_size(k: u16) -> Option<usize> {
    Some(match k {
        kind::BYTE | kind::ASCII | kind::SBYTE | kind::UNDEFINED => 1,
        kind::SHORT | kind::SSHORT => 2,
        kind::LONG | kind::SLONG | kind::FLOAT | kind::IFD => 4,
        kind::RATIONAL | kind::SRATIONAL | kind::DOUBLE => 8,
        _ => return None,
    })
}

/// A parsed IFD.
#[derive(Clone, Debug)]
pub(crate) struct Ifd {
    pub order: Order,
    pub entries: Vec<Entry>,
}

impl Ifd {
    pub fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// Integer values (BYTE, SHORT, LONG and their signed forms, IFD).
    pub fn u32s(&self, tag: u16) -> Option<Vec<u32>> {
        let e = self.get(tag)?;
        let o = self.order;
        let n = e.count as usize;
        Some(match e.kind {
            kind::BYTE | kind::UNDEFINED => e.data[..n].iter().map(|&b| u32::from(b)).collect(),
            kind::SBYTE => e.data[..n].iter().map(|&b| b as i8 as u32).collect(),
            kind::SHORT => e
                .data
                .chunks(2)
                .take(n)
                .map(|c| u32::from(o.u16(c)))
                .collect(),
            kind::SSHORT => e
                .data
                .chunks(2)
                .take(n)
                .map(|c| o.u16(c) as i16 as u32)
                .collect(),
            kind::LONG | kind::SLONG | kind::IFD => {
                e.data.chunks(4).take(n).map(|c| o.u32(c)).collect()
            }
            _ => return None,
        })
    }

    pub fn u32(&self, tag: u16) -> Option<u32> {
        self.u32s(tag)?.first().copied()
    }

    /// Numeric values of any type as `f64`.
    pub fn f64s(&self, tag: u16) -> Option<Vec<f64>> {
        let e = self.get(tag)?;
        let o = self.order;
        let n = e.count as usize;
        Some(match e.kind {
            kind::RATIONAL => e
                .data
                .chunks(8)
                .take(n)
                .map(|c| ratio(f64::from(o.u32(c)), f64::from(o.u32(&c[4..]))))
                .collect(),
            kind::SRATIONAL => e
                .data
                .chunks(8)
                .take(n)
                .map(|c| ratio(f64::from(o.u32(c) as i32), f64::from(o.u32(&c[4..]) as i32)))
                .collect(),
            kind::FLOAT => e
                .data
                .chunks(4)
                .take(n)
                .map(|c| f64::from(f32::from_bits(o.u32(c))))
                .collect(),
            kind::DOUBLE => e
                .data
                .chunks(8)
                .take(n)
                .map(|c| f64::from_bits(o.u64(c)))
                .collect(),
            kind::SBYTE | kind::SSHORT | kind::SLONG => self
                .u32s(tag)?
                .into_iter()
                .map(|v| f64::from(v as i32))
                .collect(),
            _ => self.u32s(tag)?.into_iter().map(f64::from).collect(),
        })
    }

    pub fn ascii(&self, tag: u16) -> Option<String> {
        let e = self.get(tag)?;
        let end = e.data.iter().position(|&b| b == 0).unwrap_or(e.data.len());
        Some(
            String::from_utf8_lossy(&e.data[..end])
                .trim_end()
                .to_string(),
        )
    }

    pub fn bytes(&self, tag: u16) -> Option<&[u8]> {
        self.get(tag).map(|e| e.data.as_slice())
    }
}

fn ratio(n: f64, d: f64) -> f64 {
    if d == 0.0 { 0.0 } else { n / d }
}

/// A TIFF file in memory.
pub(crate) struct Tiff<'a> {
    pub data: &'a [u8],
    pub order: Order,
    /// Offset of IFD 0.
    pub first: usize,
}

/// Most entries an IFD may have (real files have well under 200).
const MAX_ENTRIES: usize = 4096;

impl<'a> Tiff<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(malformed("shorter than a TIFF header"));
        }
        let order = match &data[..2] {
            b"II" => Order::Little,
            b"MM" => Order::Big,
            _ => return Err(malformed("not a TIFF file")),
        };
        if order.u16(&data[2..]) != 42 {
            return Err(malformed("not a TIFF file (magic)"));
        }
        let first = order.u32(&data[4..]) as usize;
        Ok(Self { data, order, first })
    }

    /// `len` bytes at `offset`, checked.
    pub fn slice(&self, offset: usize, len: usize) -> Result<&'a [u8]> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| malformed("offset overflow"))?;
        self.data
            .get(offset..end)
            .ok_or_else(|| malformed(format!("{len} bytes at {offset} beyond the file")))
    }

    /// The IFD at `offset`.
    pub fn ifd(&self, offset: usize) -> Result<Ifd> {
        let o = self.order;
        let n = o.u16(self.slice(offset, 2)?) as usize;
        if n > MAX_ENTRIES {
            return Err(malformed(format!("IFD with {n} entries")));
        }
        let raw = self.slice(offset + 2, n * 12)?;
        let mut entries = Vec::with_capacity(n);
        for e in raw.chunks(12) {
            let tag = o.u16(e);
            let k = o.u16(&e[2..]);
            let count = o.u32(&e[4..]);
            let Some(size) = type_size(k) else {
                continue;
            };
            let len = (count as usize)
                .checked_mul(size)
                .ok_or_else(|| malformed("entry size overflow"))?;
            let data = if len <= 4 {
                e[8..8 + len].to_vec()
            } else {
                match self.slice(o.u32(&e[8..]) as usize, len) {
                    Ok(d) => d.to_vec(),
                    // A damaged entry is skipped rather than failing the file.
                    Err(_) => continue,
                }
            };
            entries.push(Entry {
                tag,
                kind: k,
                count,
                data,
            });
        }
        Ok(Ifd { order: o, entries })
    }

    /// The offset of the IFD after the one at `offset` (0: none).
    pub fn next_ifd(&self, offset: usize) -> Result<usize> {
        let n = self.order.u16(self.slice(offset, 2)?) as usize;
        Ok(self.order.u32(self.slice(offset + 2 + n * 12, 4)?) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_entries_decode() {
        // MM, IFD at 8 with two entries: ImageWidth SHORT 300, a RATIONAL 1/3 at offset 38.
        let mut f = b"MM\0\x2a\0\0\0\x08".to_vec();
        f.extend_from_slice(&2u16.to_be_bytes());
        f.extend_from_slice(&256u16.to_be_bytes());
        f.extend_from_slice(&3u16.to_be_bytes());
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(&[0x01, 0x2c, 0, 0]);
        f.extend_from_slice(&33434u16.to_be_bytes());
        f.extend_from_slice(&5u16.to_be_bytes());
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(&38u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(f.len(), 38);
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(&3u32.to_be_bytes());
        let t = Tiff::parse(&f).unwrap();
        let ifd = t.ifd(t.first).unwrap();
        assert_eq!(ifd.u32(256), Some(300));
        assert!((ifd.f64s(33434).unwrap()[0] - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(t.next_ifd(t.first).unwrap(), 0);
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        assert!(Tiff::parse(b"II*").is_err());
        assert!(Tiff::parse(b"XX*\0\x08\0\0\0").is_err());
        let t = Tiff::parse(b"II*\0\xff\xff\0\0").unwrap();
        assert!(t.ifd(t.first).is_err());
        // An entry pointing past the end is skipped.
        let mut f = b"II*\0\x08\0\0\0".to_vec();
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&270u16.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&100u32.to_le_bytes());
        f.extend_from_slice(&9999u32.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        let t = Tiff::parse(&f).unwrap();
        assert!(t.ifd(8).unwrap().entries.is_empty());
    }
}

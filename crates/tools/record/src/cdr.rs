//! The little-endian ROS 2 CDR reader for the messages the Styx MCAP layout carries.
//!
//! Primitives are aligned to their size, counted from the end of the 4-byte encapsulation
//! header. Strings are a `u32` length that counts the terminating NUL. Sequences of bytes are a
//! `u32` count then the bytes. Every read is bounds checked.

use crate::error::RecordError;

const ENCAPSULATION_CDR_LE: [u8; 4] = [0x00, 0x01, 0x00, 0x00];

pub(crate) struct Cdr<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cdr<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Result<Self, RecordError> {
        if buf.get(..4) != Some(&ENCAPSULATION_CDR_LE[..]) {
            return Err(RecordError::Corrupt("message is not little-endian CDR"));
        }
        Ok(Self { buf, pos: 4 })
    }

    fn align(&mut self, size: usize) {
        let rel = self.pos - 4;
        self.pos += (size - rel % size) % size;
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], RecordError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(RecordError::Corrupt("CDR length overflows"))?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(RecordError::Corrupt("truncated CDR message"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self, align: usize) -> Result<[u8; N], RecordError> {
        self.align(align);
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.fixed::<1>(1)?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool, RecordError> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(self.fixed(4)?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, RecordError> {
        Ok(i32::from_le_bytes(self.fixed(4)?))
    }

    pub(crate) fn f32(&mut self) -> Result<f32, RecordError> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(self.fixed(8)?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, RecordError> {
        Ok(i64::from_le_bytes(self.fixed(8)?))
    }

    /// A CDR string, with its terminating NUL checked and dropped.
    pub(crate) fn string(&mut self) -> Result<&'a str, RecordError> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Err(RecordError::Corrupt("CDR string without its NUL"));
        }
        let bytes = self.take(len)?;
        let (text, nul) = bytes.split_at(len - 1);
        if nul != [0] {
            return Err(RecordError::Corrupt("CDR string without its NUL"));
        }
        core::str::from_utf8(text).map_err(|_| RecordError::Corrupt("CDR string is not UTF-8"))
    }

    /// A `uint8[]`: a `u32` count, then the bytes.
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], RecordError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// Whether every byte has been read (fields appended by later versions are absent from
    /// older messages).
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }
}

/// The little-endian ROS 2 CDR writer, the reader's mirror: primitives aligned to their size
/// from the end of the encapsulation header. It appends to a caller's buffer, so a reused
/// buffer allocates nothing once it has grown.
pub(crate) struct CdrWriter<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> CdrWriter<'a> {
    /// Clears `buf` and writes the encapsulation header to it.
    pub(crate) fn new(buf: &'a mut Vec<u8>) -> Self {
        buf.clear();
        buf.extend_from_slice(&ENCAPSULATION_CDR_LE);
        Self { buf }
    }

    fn align(&mut self, size: usize) {
        let rel = self.buf.len() - 4;
        let pad = (size - rel % size) % size;
        self.buf.resize(self.buf.len() + pad, 0);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.align(8);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.align(8);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// A string: its length with the NUL, the bytes, the NUL.
    pub(crate) fn string(&mut self, s: &str) {
        self.u32(s.len() as u32 + 1);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    /// A `uint8[]` of `len` bytes, filled by `fill` in place.
    pub(crate) fn bytes_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) {
        self.u32(len as u32);
        let start = self.buf.len();
        self.buf.resize(start + len, 0);
        fill(&mut self.buf[start..]);
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) {
        self.u32(data.len() as u32);
        self.buf.extend_from_slice(data);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cdr, CdrWriter};

    #[test]
    fn written_fields_read_back() {
        let mut buf = Vec::new();
        let mut w = CdrWriter::new(&mut buf);
        w.u8(7);
        w.u64(1 << 40);
        w.string("styx");
        w.bool(true);
        w.f32(1.5);
        w.i64(-1);
        w.bytes(&[1, 2, 3]);
        w.i32(-5);
        let mut r = Cdr::new(&buf).unwrap();
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u64().unwrap(), 1 << 40);
        assert_eq!(r.string().unwrap(), "styx");
        assert!(r.bool().unwrap());
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.i64().unwrap(), -1);
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert_eq!(r.i32().unwrap(), -5);
        assert!(r.at_end());
    }
}

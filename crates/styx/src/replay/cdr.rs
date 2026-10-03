//! Minimal ROS 2 CDR (XCDR1, little-endian) encoding for the MCAP recording messages.
//!
//! Primitives are aligned to their size, counted from the end of the 4-byte encapsulation
//! header. Strings are a `u32` length including the terminating NUL, then the bytes and NUL.
//! Sequences are a `u32` element count, then the elements.

use super::ReplayError;

const ENCAPSULATION_CDR_LE: [u8; 4] = [0x00, 0x01, 0x00, 0x00];

pub(crate) struct CdrWriter {
    buf: Vec<u8>,
}

impl CdrWriter {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut buf = Vec::with_capacity(capacity + 4);
        buf.extend_from_slice(&ENCAPSULATION_CDR_LE);
        Self { buf }
    }

    fn align(&mut self, size: usize) {
        let pos = self.buf.len() - 4;
        let pad = (size - pos % size) % size;
        self.buf.resize(self.buf.len() + pad, 0);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
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

    pub(crate) fn string(&mut self, s: &str) {
        self.u32(s.len() as u32 + 1);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) {
        self.u32(data.len() as u32);
        self.buf.extend_from_slice(data);
    }

    /// A `uint8[]` whose contents are written by `fill` into `len` bytes, avoiding a copy.
    pub(crate) fn bytes_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) {
        self.u32(len as u32);
        let start = self.buf.len();
        self.buf.resize(start + len, 0);
        fill(&mut self.buf[start..]);
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub(crate) struct CdrReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> CdrReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self, ReplayError> {
        match data.get(..4) {
            Some(header) if header == ENCAPSULATION_CDR_LE => Ok(Self { data, pos: 4 }),
            _ => Err(ReplayError::Corrupt("message is not little-endian CDR")),
        }
    }

    fn align(&mut self, size: usize) {
        let pos = self.pos - 4;
        self.pos += (size - pos % size) % size;
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ReplayError> {
        let out = self
            .data
            .get(self.pos..self.pos + len)
            .ok_or(ReplayError::Corrupt("truncated CDR message"))?;
        self.pos += len;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ReplayError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool, ReplayError> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ReplayError> {
        self.align(4);
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ReplayError> {
        self.align(4);
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn f32(&mut self) -> Result<f32, ReplayError> {
        Ok(f32::from_bits(self.u32()?))
    }

    /// Whether every byte of the message has been read (fields appended by later versions
    /// are absent from older messages).
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ReplayError> {
        self.align(8);
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, ReplayError> {
        self.align(8);
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn string(&mut self) -> Result<String, ReplayError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        String::from_utf8(bytes.to_vec()).map_err(|_| ReplayError::Corrupt("invalid utf-8"))
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], ReplayError> {
        let len = self.u32()? as usize;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_alignment() {
        let mut w = CdrWriter::with_capacity(64);
        w.u8(7);
        w.u64(1 << 40);
        w.string("mono8");
        w.bool(true);
        w.i64(-5);
        w.bytes(&[1, 2, 3]);
        w.i32(-2);
        let buf = w.finish();
        // u8 at 0, u64 padded to 8.
        assert_eq!(&buf[4..5], &[7]);
        assert_eq!(&buf[12..20], &(1u64 << 40).to_le_bytes());
        let mut r = CdrReader::new(&buf).unwrap();
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u64().unwrap(), 1 << 40);
        assert_eq!(r.string().unwrap(), "mono8");
        assert!(r.bool().unwrap());
        assert_eq!(r.i64().unwrap(), -5);
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert_eq!(r.i32().unwrap(), -2);
    }
}

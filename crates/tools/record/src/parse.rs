//! A bounds-checked reader over a byte slice. Every read says what it needs and fails cleanly.

use std::collections::BTreeMap;

use crate::error::RecordError;

pub(crate) struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The next `n` bytes. Fails rather than reading past the end.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], RecordError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(RecordError::Corrupt("length overflows"))?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(RecordError::Corrupt("field runs past its record"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], RecordError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, RecordError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Everything not read yet.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let out = self.buf.get(self.pos..).unwrap_or(&[]);
        self.pos = self.buf.len();
        out
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Fails when bytes are left over: a record longer than its fields.
    pub(crate) fn finish(&self) -> Result<(), RecordError> {
        if self.at_end() {
            Ok(())
        } else {
            Err(RecordError::Corrupt(
                "trailing bytes after a record's fields",
            ))
        }
    }

    /// An MCAP string: a `u32` length, then UTF-8 bytes (no terminator).
    pub(crate) fn mcap_string(&mut self) -> Result<String, RecordError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| RecordError::Corrupt("MCAP string is not UTF-8"))
    }

    /// An MCAP key-value map: a `u32` byte length, then string pairs filling it.
    pub(crate) fn mcap_map(&mut self) -> Result<BTreeMap<String, String>, RecordError> {
        let len = self.u32()? as usize;
        let mut inner = Parser::new(self.take(len)?);
        let mut map = BTreeMap::new();
        while !inner.at_end() {
            let key = inner.mcap_string()?;
            let value = inner.mcap_string()?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

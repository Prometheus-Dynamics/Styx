//! Bit reading over an RBSP (emulation-prevention bytes removed), Annex B NAL
//! unit splitting, and the mapping from an RBSP offset back to the raw bytes.

use core::fmt;

/// Why a bitstream could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data ended inside a syntax element.
    Truncated,
    /// The stream is valid HEVC, but this parser does not handle the feature.
    Unsupported(&'static str),
    /// A value is outside the range the specification allows.
    Invalid(&'static str),
    /// The output buffer given to [`unescape`] is too small.
    Buffer,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("the bitstream ends inside a syntax element"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Invalid(what) => write!(f, "invalid: {what}"),
            Error::Buffer => f.write_str("the output buffer is too small"),
        }
    }
}

/// The result of a parse.
pub type Result<T> = core::result::Result<T, Error>;

/// Reads bits, most significant first, from an RBSP.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader at the first bit of `data`.
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The position in bits from the start of the data.
    pub const fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Bits left before the end of the data.
    pub const fn bits_left(&self) -> usize {
        // `pos` never passes the end, and a slice's bit count fits in `usize`
        // on every target this builds for; saturating keeps it a non-panic.
        self.data.len().saturating_mul(8).saturating_sub(self.pos)
    }

    /// One bit as a flag.
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bits(1)? == 1)
    }

    /// The next `n` bits (at most 32) as an unsigned value.
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::Invalid("more than 32 bits at once"));
        }
        if (n as usize) > self.bits_left() {
            return Err(Error::Truncated);
        }
        let mut value = 0u64;
        for _ in 0..n {
            // In range: `bits_left` covered every bit read here.
            let byte = *self.data.get(self.pos >> 3).ok_or(Error::Truncated)?;
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            value = (value << 1) | u64::from(bit);
            self.pos += 1;
        }
        Ok(value as u32)
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if n > self.bits_left() {
            return Err(Error::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    /// An unsigned Exp-Golomb value, `ue(v)`.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("an Exp-Golomb code longer than 32 bits"));
            }
        }
        let value = (1u64 << zeros) - 1 + u64::from(self.bits(zeros)?);
        u32::try_from(value).map_err(|_| Error::Invalid("an Exp-Golomb value above 32 bits"))
    }

    /// An unsigned Exp-Golomb value that must fit in `max` (inclusive).
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let value = self.ue()?;
        if value > max {
            return Err(Error::Invalid(what));
        }
        Ok(value)
    }

    /// A signed Exp-Golomb value that must lie in `min..=max`.
    pub fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32> {
        let value = self.se()?;
        if value < min || value > max {
            return Err(Error::Invalid(what));
        }
        Ok(value)
    }

    /// A signed Exp-Golomb value, `se(v)`.
    pub fn se(&mut self) -> Result<i32> {
        let k = i64::from(self.ue()?);
        let value = if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(value).map_err(|_| Error::Invalid("a signed Exp-Golomb value above 32 bits"))
    }

    /// Skips to the next byte boundary, which must be `byte_alignment()`: a one
    /// bit and zeros. A zero bit where the one belongs is an error.
    pub fn byte_alignment(&mut self) -> Result<()> {
        if !self.flag()? {
            return Err(Error::Invalid("byte_alignment() without its one bit"));
        }
        while self.pos & 7 != 0 {
            if self.flag()? {
                return Err(Error::Invalid("a one bit in byte_alignment() padding"));
            }
        }
        Ok(())
    }

    /// The byte offset of the reader, once it is byte aligned.
    pub const fn byte_offset(&self) -> usize {
        self.pos >> 3
    }
}

/// One NAL unit of an Annex B byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// `nal_unit_type` (6 bits): 32 VPS, 33 SPS, 34 PPS, 19 and 20 IDR, and so on.
    pub unit_type: u8,
    /// `nuh_layer_id`.
    pub layer_id: u8,
    /// `nuh_temporal_id_plus1`.
    pub temporal_id_plus1: u8,
    /// The unit's bytes: its two-byte header and payload, with emulation
    /// prevention bytes still in place. No start code, no trailing zeros.
    pub raw: &'a [u8],
}

/// Iterates the NAL units of an Annex B byte stream (start codes `00 00 01`).
/// Units with fewer than two bytes are skipped.
pub fn nal_units(stream: &[u8]) -> NalUnits<'_> {
    NalUnits { stream, pos: 0 }
}

/// The iterator behind [`nal_units`].
#[derive(Clone, Debug)]
pub struct NalUnits<'a> {
    stream: &'a [u8],
    pos: usize,
}

/// The index just past the next `00 00 01` at or after `from`, and the index
/// of its first zero.
fn next_start_code(stream: &[u8], from: usize) -> Option<(usize, usize)> {
    let at = stream
        .get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])?;
    let i = from + at;
    Some((i, i + 3))
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = NalUnit<'a>;

    fn next(&mut self) -> Option<NalUnit<'a>> {
        loop {
            let (_, start) = next_start_code(self.stream, self.pos)?;
            let end = match next_start_code(self.stream, start) {
                Some((next, _)) => next,
                None => self.stream.len(),
            };
            // A four-byte start code, and trailing_zero_8bits, leave zeros
            // before the next start code: they are not part of this unit.
            self.pos = end;
            let unit = self.stream.get(start..end)?;
            let kept = unit
                .iter()
                .rposition(|&b| b != 0)
                .map_or(0, |last| last + 1);
            let raw = unit.get(..kept)?;
            let [first, second, ..] = *raw else {
                continue;
            };
            return Some(NalUnit {
                unit_type: (first >> 1) & 0x3f,
                layer_id: ((first & 1) << 5) | (second >> 3),
                temporal_id_plus1: second & 7,
                raw,
            });
        }
    }
}

/// Removes the emulation prevention bytes (`00 00 03` gives `00 00`) from
/// `raw` into `out`, and returns the number of bytes written. The result is
/// the RBSP, header included.
pub fn unescape(raw: &[u8], out: &mut [u8]) -> Result<usize> {
    let mut written = 0;
    let mut zeros = 0;
    for &byte in raw {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        *out.get_mut(written).ok_or(Error::Buffer)? = byte;
        written += 1;
    }
    Ok(written)
}

/// The index in `raw` of the byte that is RBSP byte `rbsp_index` (the same
/// byte after [`unescape`]), counting the emulation prevention bytes before it.
/// `None` when `raw` has fewer bytes than that.
pub fn raw_index(raw: &[u8], rbsp_index: usize) -> Option<usize> {
    let mut seen = 0;
    let mut zeros = 0;
    for (i, &byte) in raw.iter().enumerate() {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        if seen == rbsp_index {
            return Some(i);
        }
        seen += 1;
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    None
}

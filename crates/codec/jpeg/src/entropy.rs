//! Baseline Huffman coding of quantised blocks (T.81 F.1.2) into a byte-stuffed bit stream, and
//! the byte sinks the encoder writes to: a `Vec` that grows, or a caller's fixed buffer that
//! refuses what does not fit.

use crate::tables::{HuffSpec, ZIGZAG};

/// Where encoded bytes go.
pub(crate) trait ByteSink {
    fn push_bytes(&mut self, bytes: &[u8]);
}

impl ByteSink for Vec<u8> {
    #[inline]
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// A caller's buffer. Bytes past its end are counted as an overflow, not written.
pub(crate) struct Fixed<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> Fixed<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }

    /// Bytes written, `None` when the data did not fit.
    pub(crate) fn written(&self) -> Option<usize> {
        (!self.overflow).then_some(self.len)
    }
}

impl ByteSink for Fixed<'_> {
    #[inline]
    fn push_bytes(&mut self, bytes: &[u8]) {
        match self.buf.get_mut(self.len..self.len + bytes.len()) {
            Some(dst) => dst.copy_from_slice(bytes),
            None => self.overflow = true,
        }
        self.len += bytes.len();
    }
}

/// Code and length of every symbol of one table (T.81 Annex C).
#[derive(Debug)]
pub(crate) struct HuffCodes {
    code: [u16; 256],
    len: [u8; 256],
}

impl HuffCodes {
    pub(crate) fn new(spec: &HuffSpec) -> Self {
        let mut out = Self {
            code: [0; 256],
            len: [0; 256],
        };
        let (mut code, mut k) = (0u16, 0);
        for (len, &count) in (1u8..).zip(&spec.bits) {
            for _ in 0..count {
                let sym = usize::from(spec.vals[k]);
                (out.code[sym], out.len[sym]) = (code, len);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        out
    }
}

/// Bits written most significant first into a sink, a 0x00 after every 0xFF.
pub(crate) struct BitWriter<'a, S: ByteSink> {
    out: &'a mut S,
    acc: u64,
    bits: u32,
}

impl<'a, S: ByteSink> BitWriter<'a, S> {
    pub(crate) fn new(out: &'a mut S) -> Self {
        Self {
            out,
            acc: 0,
            bits: 0,
        }
    }

    /// The low `len` bits of `value` (at most 32 at a time).
    #[inline]
    fn put(&mut self, value: u32, len: u32) {
        self.acc = (self.acc << len) | u64::from(value);
        self.bits += len;
        while self.bits >= 8 {
            self.bits -= 8;
            let byte = (self.acc >> self.bits) as u8;
            self.out.push_bytes(&[byte]);
            if byte == 0xFF {
                self.out.push_bytes(&[0]);
            }
        }
    }

    /// Pads the last byte with one bits (T.81 F.1.2.3), leaving the stream on a byte boundary.
    pub(crate) fn flush(&mut self) {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.put((1 << pad) - 1, pad);
        }
    }

    /// A marker, written as it is (after [`Self::flush`]).
    pub(crate) fn marker(&mut self, marker: u8) {
        debug_assert_eq!(self.bits, 0, "a marker on a byte boundary only");
        self.out.push_bytes(&[0xFF, marker]);
    }

    /// `symbol`'s code followed by `extra`'s low `size` bits.
    #[inline]
    fn symbol(&mut self, table: &HuffCodes, symbol: u8, extra: u32, size: u32) {
        let s = usize::from(symbol);
        let len = u32::from(table.len[s]);
        self.put((u32::from(table.code[s]) << size) | extra, len + size);
    }

    /// One block of quantised coefficients (natural order), its DC coded against `pred`
    /// (updated).
    pub(crate) fn block(&mut self, coefs: &[i16], pred: &mut i16, dc: &HuffCodes, ac: &HuffCodes) {
        // Baseline: DC differences fit 11 bits, AC values 10.
        let value = i32::from(coefs[0]).clamp(-2047, 2047);
        let diff = value - i32::from(*pred);
        *pred = value as i16;
        let (size, extra) = magnitude(diff);
        self.symbol(dc, size as u8, extra, size);
        let mut run = 0;
        for &k in &ZIGZAG[1..] {
            let v = i32::from(coefs[usize::from(k)]).clamp(-1023, 1023);
            if v == 0 {
                run += 1;
                continue;
            }
            while run >= 16 {
                self.symbol(ac, 0xF0, 0, 0);
                run -= 16;
            }
            let (size, extra) = magnitude(v);
            self.symbol(ac, ((run << 4) | size) as u8, extra, size);
            run = 0;
        }
        if run > 0 {
            self.symbol(ac, 0x00, 0, 0);
        }
    }
}

/// The size category of `v` and its extra bits (negative values one less, T.81 F.1.2.1).
#[inline]
fn magnitude(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let extra = if v < 0 { v - 1 } else { v } as u32 & ((1u32 << size) - 1);
    (size, extra)
}

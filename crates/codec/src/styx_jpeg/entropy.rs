//! Baseline Huffman coding of quantised blocks (T.81 F.1.2) into a byte-stuffed bit stream.

use super::tables::{HuffSpec, ZIGZAG};

/// Code and length of every symbol of one table (T.81 Annex C).
pub(super) struct HuffCodes {
    code: [u16; 256],
    len: [u8; 256],
}

impl HuffCodes {
    pub(super) fn new(spec: &HuffSpec) -> Self {
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

/// Bits written most significant first into `out`, a 0x00 after every 0xFF.
pub(super) struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    bits: u32,
}

impl<'a> BitWriter<'a> {
    pub(super) fn new(out: &'a mut Vec<u8>) -> Self {
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
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
        }
    }

    /// Pads the last byte with one bits (T.81 F.1.2.3).
    pub(super) fn flush(&mut self) {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.put((1 << pad) - 1, pad);
        }
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
    pub(super) fn block(&mut self, coefs: &[i16], pred: &mut i16, dc: &HuffCodes, ac: &HuffCodes) {
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

//! RBSP bit writing (fixed-width, Exp-Golomb) and NAL unit framing with emulation prevention.

/// An RBSP being written, MSB first, into a reused buffer.
#[derive(Debug, Default)]
pub(super) struct Bits {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    /// Starts a new RBSP (the buffer's capacity kept).
    pub(super) fn clear(&mut self) {
        self.buf.clear();
        self.acc = 0;
        self.n = 0;
    }

    /// The low `n` (at most 32) bits of `v`.
    pub(super) fn put(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32 && (n == 32 || v >> n == 0));
        if n == 0 {
            return;
        }
        self.acc = self.acc << n | u64::from(v);
        self.n += n;
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.acc >> self.n) as u8);
        }
    }

    pub(super) fn flag(&mut self, b: bool) {
        self.put(u32::from(b), 1);
    }

    /// `ue(v)`.
    pub(super) fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(0, len - 1);
        if len > 32 {
            self.put((x >> 32) as u32, len - 32);
            self.put(x as u32, 32);
        } else {
            self.put(x as u32, len);
        }
    }

    /// `se(v)`.
    pub(super) fn se(&mut self, v: i32) {
        let k = if v > 0 {
            2 * v.unsigned_abs() - 1
        } else {
            2 * v.unsigned_abs()
        };
        self.ue(k);
    }

    /// `rbsp_trailing_bits()`: a one, then zeros to the byte.
    pub(super) fn trailing(&mut self) {
        self.put(1, 1);
        let pad = (8 - self.n % 8) % 8;
        self.put(0, pad);
    }

    /// The bytes so far (whole bytes only; call after [`Self::trailing`]).
    pub(super) fn bytes(&self) -> &[u8] {
        debug_assert_eq!(self.n, 0);
        &self.buf
    }
}

/// Appends a NAL unit to `out`: a four-byte start code, the header (`nal_ref_idc`,
/// `nal_unit_type`), then `rbsp` with an emulation prevention byte before any 0..=3 that
/// follows two zeros.
pub(super) fn nal(out: &mut Vec<u8>, ref_idc: u8, kind: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, ref_idc << 5 | kind]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes() {
        let mut b = Bits::default();
        // ue: 0 → 1, 1 → 010, 2 → 011, 3 → 00100; se: 1 → 010, -1 → 011.
        for v in [0, 1, 2, 3] {
            b.ue(v);
        }
        b.se(1);
        b.se(-1);
        b.trailing();
        // 1 010 011 00100 010 011 1 + pad: 1010 0110 0100 0100 1110 0000
        assert_eq!(b.bytes(), &[0xa6, 0x44, 0xe0]);
        b.clear();
        b.ue(u32::MAX - 1);
        b.trailing();
        assert_eq!(b.bytes().len(), 8);
    }

    #[test]
    fn emulation_prevention() {
        let mut out = Vec::new();
        nal(&mut out, 3, 7, &[0, 0, 1, 0, 0, 0, 0, 0, 4]);
        assert_eq!(out, [0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 4]);
    }
}

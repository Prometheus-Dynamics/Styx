//! CRC-32 (IEEE 802.3, as zlib and MCAP use), slice-by-8: eight tables built at compile time,
//! eight bytes per step.

const TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][i] = c;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[t - 1][i];
            tables[t][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

/// The CRC-32 of `data`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    Crc32::new().update(data).value()
}

/// A CRC-32 over several pieces, the same as [`crc32`] of them joined.
#[derive(Clone, Copy)]
pub(crate) struct Crc32(u32);

impl Crc32 {
    pub(crate) const fn new() -> Self {
        Self(!0)
    }

    #[must_use]
    pub(crate) fn update(mut self, data: &[u8]) -> Self {
        let (blocks, tail) = data.as_chunks::<8>();
        for block in blocks {
            let lo = self.0 ^ u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
            let hi = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
            self.0 = TABLES[7][(lo & 0xFF) as usize]
                ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
                ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
                ^ TABLES[4][(lo >> 24) as usize]
                ^ TABLES[3][(hi & 0xFF) as usize]
                ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
                ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
                ^ TABLES[0][(hi >> 24) as usize];
        }
        for &b in tail {
            self.0 = TABLES[0][((self.0 ^ u32::from(b)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
        self
    }

    pub(crate) const fn value(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        let split = super::Crc32::new().update(b"1234").update(b"56789").value();
        assert_eq!(split, 0xCBF4_3926);
    }

    /// Slice-by-8 against the bit-at-a-time definition, at every length and split up to 300.
    #[test]
    fn matches_the_definition() {
        fn bitwise(data: &[u8]) -> u32 {
            let mut c = !0u32;
            for &b in data {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let data: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in 0..data.len() {
            assert_eq!(crc32(&data[..len]), bitwise(&data[..len]), "length {len}");
            let split = len / 3;
            let joined = super::Crc32::new()
                .update(&data[..split])
                .update(&data[split..len])
                .value();
            assert_eq!(joined, bitwise(&data[..len]), "split {split} of {len}");
        }
    }
}

//! CAVLC residual blocks (9.2): `coeff_token`, trailing-one signs, levels, `total_zeros`
//! and `run_before`, the level coding as OpenH264's `WriteBlockResidualCavlc` does it.

use super::bits::Bits;
use super::tables::{COEFF_TOKEN, NC_CLASS, RUN_BEFORE, TOTAL_ZEROS, TOTAL_ZEROS_CHROMA_DC};

/// Writes one residual block: `coeffs` in scan order (16 for a 4x4 block, 15 for AC, 4 for
/// chroma DC), `nc` the predicted count (-1 for chroma DC). Returns `TotalCoeff`.
pub(super) fn block(bits: &mut Bits, coeffs: &[i32], nc: i32) -> u8 {
    let max = coeffs.len();
    // Nonzero levels from the highest frequency down, with the zeros below each.
    let mut level = [0i32; 16];
    let mut run = [0u8; 16];
    let mut total = 0usize;
    let mut total_zeros = 0u32;
    let mut started = false;
    for &c in coeffs.iter().rev() {
        if c != 0 {
            level[total] = c;
            total += 1;
            started = true;
        } else if started {
            run[total - 1] += 1;
            total_zeros += 1;
        }
    }
    let class = usize::from(NC_CLASS[if nc < 0 { 17 } else { nc.min(16) as usize }]);
    let mut t1s = 0usize;
    let mut signs = 0u32;
    while t1s < total.min(3) && level[t1s].abs() == 1 {
        signs = signs << 1 | u32::from(level[t1s] < 0);
        t1s += 1;
    }
    let (code, len) = COEFF_TOKEN[class][total][t1s];
    if total == 0 {
        bits.put(u32::from(code), u32::from(len));
        return 0;
    }
    bits.put(u32::from(code) << t1s | signs, u32::from(len) + t1s as u32);

    let mut suffix_len = u32::from(total > 10 && t1s < 3);
    for (i, &v) in level.iter().enumerate().take(total).skip(t1s) {
        let mut code = if v > 0 { 2 * v - 2 } else { -2 * v - 1 };
        if i == t1s && t1s < 3 {
            code -= 2;
        }
        let mut prefix = code >> suffix_len;
        let mut suffix_size = suffix_len;
        let mut suffix = code - (prefix << suffix_len);
        if (14..30).contains(&prefix) && suffix_len == 0 {
            prefix = 14;
            suffix = code - 14;
            suffix_size = 4;
        } else if prefix >= 15 {
            prefix = 15;
            suffix = code - (15 << suffix_len);
            if suffix_len == 0 {
                suffix -= 15;
            }
            // Levels are clamped so Baseline's 12-bit escape always fits.
            debug_assert!(suffix < 1 << 12, "level {v} too large for Baseline");
            suffix_size = 12;
        }
        bits.put(
            (1 << suffix_size) | suffix as u32,
            prefix as u32 + 1 + suffix_size,
        );
        if suffix_len == 0 {
            suffix_len = 1;
        }
        if v.unsigned_abs() > 3 << (suffix_len - 1) && suffix_len < 6 {
            suffix_len += 1;
        }
    }

    if total < max {
        let (code, len) = if max == 4 {
            TOTAL_ZEROS_CHROMA_DC[total][total_zeros as usize]
        } else {
            TOTAL_ZEROS[total][total_zeros as usize]
        };
        bits.put(u32::from(code), u32::from(len));
    }
    let mut zeros_left = total_zeros as usize;
    for &r in run.iter().take(total - 1) {
        if zeros_left == 0 {
            break;
        }
        let (code, len) = RUN_BEFORE[zeros_left.min(7)][usize::from(r)];
        bits.put(u32::from(code), u32::from(len));
        zeros_left -= usize::from(r);
    }
    total as u8
}

/// The predicted coefficient count from the left (`a`) and top (`b`) blocks' counts.
pub(super) fn predict_nc(a: Option<u8>, b: Option<u8>) -> i32 {
    match (a, b) {
        (Some(a), Some(b)) => (i32::from(a) + i32::from(b) + 1) >> 1,
        (Some(n), None) | (None, Some(n)) => i32::from(n),
        (None, None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_of(coeffs: &[i32], nc: i32) -> String {
        let mut b = Bits::default();
        block(&mut b, coeffs, nc);
        b.trailing();
        let s: String = b.bytes().iter().map(|x| format!("{x:08b}")).collect();
        // Drop the trailing one and its padding.
        s[..s.rfind('1').unwrap()].to_string()
    }

    #[test]
    fn the_standard_example() {
        // The textbook 4x4 example (Richardson): 0 3 -1 0 / 0 -1 1 0 / 1 0 0 0 / 0 0 0 0 in
        // zig-zag order is 0,3,0,1,-1,-1,0,1,0..., nC 0: coeff_token 0000100 (5 coeffs, 3
        // trailing ones), signs 0 1 1, levels 1 (+1) and 0010 (+3), total_zeros 111 (3),
        // run_before 10 1 1 01.
        let z = [0, 3, 0, 1, -1, -1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(bits_of(&z, 0), "000010001110010111101101");
    }

    #[test]
    fn empty_and_chroma_dc() {
        assert_eq!(bits_of(&[0; 16], 0), "1");
        assert_eq!(bits_of(&[0; 16], 4), "1111");
        assert_eq!(bits_of(&[0; 4], -1), "01");
    }
}

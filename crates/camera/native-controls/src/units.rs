//! Conversions between styx-controls units and a sensor's exposure codes. Pure arithmetic on the
//! mode's numbers; nothing here knows a chip. Gain is in `gain.rs`, which takes the contract's law.
//! Frame lengths and frame rates are the contract's (`frame_period_ps`, `fps_milli_of`,
//! `frame_length_for_fps_milli`): nothing here computes a period or a rate.

/// The picoseconds in a microsecond.
const PS_PER_US: u64 = 1_000_000;

/// Exposure code (lines times `2^fraction_bits`) for `us` microseconds, rounded to the nearest code.
pub fn exposure_code(us: u32, line_time_ps: u64, fraction_bits: u8) -> u64 {
    let scale = PS_PER_US << fraction_bits;
    let num = u64::from(us) * scale;
    (num + line_time_ps / 2) / line_time_ps
}

/// Microseconds for an exposure code, rounded to the nearest microsecond.
pub fn exposure_us(code: u64, line_time_ps: u64, fraction_bits: u8) -> u32 {
    let den = PS_PER_US << fraction_bits;
    let num = code * line_time_ps;
    u32::try_from((num + den / 2) / den).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LT: u64 = 9_100_000; // 9.1 us per line: a 728-clock line at 80 MHz.

    #[test]
    fn exposure_round_trips_to_the_microsecond() {
        for us in [1, 5, 100, 999, 8_000, 12_000, 16_000, 250_000] {
            let code = exposure_code(us, LT, 4);
            assert_eq!(exposure_us(code, LT, 4), us, "us {us} code {code}");
        }
    }

    #[test]
    fn exposure_code_counts_sixteenths_of_a_line() {
        // 8000 us at 9.1 us a line is 879.12 lines, 14066 sixteenths.
        assert_eq!(exposure_code(8_000, LT, 4), 14_066);
        assert_eq!(exposure_code(8_000, LT, 0), 879);
    }
}

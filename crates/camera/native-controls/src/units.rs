//! Conversions between styx-controls units and a sensor's codes. Pure arithmetic on the mode's
//! numbers; nothing here knows a chip. Gain is in `gain.rs`, which takes the contract's law.

/// The picoseconds in a microsecond.
const PS_PER_US: u64 = 1_000_000;
/// The picoseconds in a second, in units of millihertz: `1e15 / mhz` is a frame duration in ps.
const PS_MHZ: u64 = 1_000_000_000_000_000;

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

/// Largest exposure code a frame of `frame_length` lines allows: the frame less `margin` lines.
pub fn exposure_max_code(frame_length: u64, margin: u32, fraction_bits: u8) -> u64 {
    frame_length.saturating_sub(u64::from(margin)) << fraction_bits
}

/// Frame length in lines for `mhz` millihertz at `line_time_ps` per line, rounded to the nearest
/// whole line. `None` for a frame rate of zero. The frame rate is the frame length over the line
/// time, so this is the length the rate asks for; the sensor's limits clamp it.
pub fn frame_length(mhz: u32, line_time_ps: u64) -> Option<u64> {
    let period = u64::from(mhz) * line_time_ps;
    if period == 0 {
        return None;
    }
    Some((PS_MHZ + period / 2) / period)
}

/// Frame rate in millihertz for a frame of `lines` lines, rounded. `0` lines gives 0.
pub fn frame_rate_milli(lines: u64, line_time_ps: u64) -> u32 {
    let period = lines * line_time_ps;
    if period == 0 {
        return 0;
    }
    u32::try_from((PS_MHZ + period / 2) / period).unwrap_or(u32::MAX)
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

    #[test]
    fn exposure_max_is_the_frame_less_the_margin() {
        assert_eq!(exposure_max_code(1822, 25, 4), 1797 << 4);
        assert_eq!(exposure_max_code(10, 25, 4), 0);
    }

    #[test]
    fn frame_length_is_the_frame_rate_over_the_line_time() {
        // 60 fps at 9.1 us a line: 1832.0 lines, and 1832 lines is 59.984 fps.
        let lines = frame_length(60_000, LT).unwrap();
        assert_eq!(lines, 1832);
        assert_eq!(frame_rate_milli(lines, LT), 59_984);
        assert_eq!(frame_length(0, LT), None);
        assert_eq!(frame_rate_milli(0, LT), 0);
    }
}

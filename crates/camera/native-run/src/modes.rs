//! Choosing a sensor mode for a request, and the pixel format the mode's frames have.

use std::cmp::Reverse;
use std::fmt;

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_contract_camera_sensor::Mode;
use styx_format::FourCc;

/// What a caller needs from a sensor mode. A mode fits when it is at least this big, at least this
/// fast, and matches each field that is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Request {
    pub width: u32,
    pub height: u32,
    /// Frames per second in thousandths (a rate of n frames a second is n * 1000). 0 accepts any rate.
    pub min_fps_milli: u32,
    /// Bits per pixel on the link, if it matters.
    pub bit_depth: Option<u8>,
    /// Colour order, if it matters.
    pub bayer: Option<BayerOrder>,
}

/// Why no mode was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeError {
    /// The sensor reports no modes.
    NoModes,
    /// No mode fits the request.
    NoMode(Request),
    /// The mode's colour order and bit depth have no frame format here.
    NoFourCc { bayer: BayerOrder, bit_depth: u8 },
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModes => write!(f, "the sensor reports no modes"),
            Self::NoMode(r) => write!(
                f,
                "no sensor mode is at least {}x{} at {} mfps{}{}",
                r.width,
                r.height,
                r.min_fps_milli,
                r.bit_depth.map_or(String::new(), |b| format!(", {b} bits")),
                r.bayer.map_or(String::new(), |b| format!(", {b:?}")),
            ),
            Self::NoFourCc { bayer, bit_depth } => {
                write!(f, "no frame format for {bayer:?} at {bit_depth} bits")
            }
        }
    }
}

impl std::error::Error for ModeError {}

/// The fastest rate a mode runs at, in thousandths of a frame per second: its shortest frame
/// (`frame_length_min` lines) over its line time. `0` for a mode with no line time or no frame.
pub fn fastest_fps_milli(mode: &Mode) -> u32 {
    let period = u64::from(mode.frame_length_min) * u64::from(mode.line_time_ps);
    if period == 0 {
        return 0;
    }
    u32::try_from((FPS_MILLI_PS + period / 2) / period).unwrap_or(u32::MAX)
}

/// Thousandths of a frame per second times picoseconds per line times lines: `1e15`.
const FPS_MILLI_PS: u64 = 1_000_000_000_000_000;

fn fits(mode: &Mode, request: &Request) -> bool {
    mode.link.width >= request.width
        && mode.link.height >= request.height
        && fastest_fps_milli(mode) >= request.min_fps_milli
        && request.bit_depth.is_none_or(|b| mode.link.bit_depth == b)
        && request.bayer.is_none_or(|b| mode.bayer == b)
}

/// The index of the mode to use for `request`: among the modes that fit, the smallest frame, then
/// the fastest, then the lowest index. The caller gets the mode's own size, which may be larger
/// than asked for.
pub fn select_mode(modes: &[Mode], request: &Request) -> Result<u16, ModeError> {
    if modes.is_empty() {
        return Err(ModeError::NoModes);
    }
    modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| fits(mode, request))
        .min_by_key(|(index, mode)| {
            let area = u64::from(mode.link.width) * u64::from(mode.link.height);
            (area, Reverse(fastest_fps_milli(mode)), *index)
        })
        .and_then(|(index, _)| u16::try_from(index).ok())
        .ok_or(ModeError::NoMode(*request))
}

/// The frame format a mode's packed frames have: its colour order at its bit depth.
pub fn fourcc_for(mode: &Mode) -> Result<FourCc, ModeError> {
    let bit_depth = mode.link.bit_depth;
    let code = match (mode.bayer, bit_depth) {
        (BayerOrder::Rggb, 8) => FourCc::RGGB,
        (BayerOrder::Grbg, 8) => FourCc::GRBG,
        (BayerOrder::Gbrg, 8) => FourCc::GBRG,
        (BayerOrder::Bggr, 8) => FourCc::BGGR,
        (BayerOrder::Rggb, 10) => FourCc::RGGB10P,
        (BayerOrder::Grbg, 10) => FourCc::GRBG10P,
        (BayerOrder::Gbrg, 10) => FourCc::GBRG10P,
        (BayerOrder::Bggr, 10) => FourCc::BGGR10P,
        (BayerOrder::Rggb, 12) => FourCc::RGGB12P,
        (BayerOrder::Grbg, 12) => FourCc::GRBG12P,
        (BayerOrder::Gbrg, 12) => FourCc::GBRG12P,
        (BayerOrder::Bggr, 12) => FourCc::BGGR12P,
        (bayer, bit_depth) => return Err(ModeError::NoFourCc { bayer, bit_depth }),
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemnos_contract_camera_receiver::LinkConfig;

    /// A mode whose shortest frame gives `fps` frames a second at 9.1 us a line, to the nearest
    /// whole line.
    fn mode(width: u32, height: u32, fps: u32, bayer: BayerOrder, bits: u8) -> Mode {
        let line_time_ps = 9_100_000;
        let frame_length_min = (1_000_000_000_000_000u64
            + u64::from(fps) * 1_000 * line_time_ps / 2)
            / (u64::from(fps) * 1_000 * line_time_ps);
        Mode {
            link: LinkConfig {
                width,
                height,
                bit_depth: bits,
                ..LinkConfig::default()
            },
            bayer,
            line_time_ps: line_time_ps as u32,
            frame_length_min: frame_length_min as u32,
            ..Mode::default()
        }
    }

    #[test]
    fn the_fastest_rate_is_the_shortest_frame_over_the_line_time() {
        // 1832 lines of 9.1 us is 59.984 fps: the mode's own numbers, not a nominal rate.
        let m = Mode {
            line_time_ps: 9_100_000,
            frame_length_min: 1832,
            ..Mode::default()
        };
        assert_eq!(fastest_fps_milli(&m), 59_984);
        assert_eq!(fastest_fps_milli(&Mode::default()), 0);
    }

    #[test]
    fn a_mode_whose_shortest_frame_is_under_the_request_does_not_fit() {
        // Shortest frame 114.80 fps: a request for 120 fps does not fit this mode.
        let slow = Mode {
            line_time_ps: 8_710_801,
            frame_length_min: 1000,
            link: LinkConfig {
                width: 320,
                height: 240,
                bit_depth: 10,
                ..LinkConfig::default()
            },
            bayer: BayerOrder::Rggb,
            ..Mode::default()
        };
        assert_eq!(fastest_fps_milli(&slow), 114_800);
        let request = Request {
            width: 320,
            height: 240,
            min_fps_milli: 120_000,
            ..Request::default()
        };
        assert_eq!(
            select_mode(&[slow], &request),
            Err(ModeError::NoMode(request))
        );
    }

    fn modes() -> Vec<Mode> {
        vec![
            mode(640, 480, 60, BayerOrder::Rggb, 10),
            mode(320, 240, 120, BayerOrder::Rggb, 10),
            mode(320, 240, 30, BayerOrder::Rggb, 8),
            mode(1280, 720, 30, BayerOrder::Gbrg, 10),
        ]
    }

    #[test]
    fn the_smallest_mode_that_fits_wins_and_the_fastest_of_equal_size() {
        let request = Request {
            width: 320,
            height: 240,
            min_fps_milli: 60_000,
            ..Request::default()
        };
        assert_eq!(select_mode(&modes(), &request), Ok(1));
        // Two 320x240 modes fit: the faster one wins the tie.
        let request = Request {
            width: 300,
            height: 200,
            min_fps_milli: 0,
            ..Request::default()
        };
        assert_eq!(select_mode(&modes(), &request), Ok(1));
        let eight_bit = Request {
            bit_depth: Some(8),
            ..request
        };
        assert_eq!(select_mode(&modes(), &eight_bit), Ok(2));
    }

    #[test]
    fn format_and_rate_filter_the_candidates() {
        let request = Request {
            width: 320,
            height: 240,
            min_fps_milli: 0,
            bit_depth: Some(10),
            bayer: Some(BayerOrder::Rggb),
        };
        assert_eq!(select_mode(&modes(), &request), Ok(1));
        let too_fast = Request {
            min_fps_milli: 200_000,
            ..request
        };
        assert_eq!(
            select_mode(&modes(), &too_fast),
            Err(ModeError::NoMode(too_fast))
        );
    }

    #[test]
    fn no_modes_and_no_fit_are_errors() {
        let request = Request::default();
        assert_eq!(select_mode(&[], &request), Err(ModeError::NoModes));
        let big = Request {
            width: 4000,
            height: 3000,
            ..Request::default()
        };
        assert_eq!(select_mode(&modes(), &big), Err(ModeError::NoMode(big)));
    }

    #[test]
    fn bayer_and_depth_map_to_packed_formats() {
        assert_eq!(
            fourcc_for(&mode(1, 1, 1, BayerOrder::Rggb, 10)),
            Ok(FourCc::RGGB10P)
        );
        assert_eq!(
            fourcc_for(&mode(1, 1, 1, BayerOrder::Bggr, 8)),
            Ok(FourCc::BGGR)
        );
        assert_eq!(
            fourcc_for(&mode(1, 1, 1, BayerOrder::Mono, 10)),
            Err(ModeError::NoFourCc {
                bayer: BayerOrder::Mono,
                bit_depth: 10
            })
        );
    }
}

//! Choosing a sensor mode for a request, and the pixel format the mode's frames have.
//!
//! The rule, in order:
//!
//! 1. A mode is a candidate when it is at least the asked size, its colour order matches if one
//!    is asked, and its frame format (or bit depth) matches if one is asked. Asking for a format
//!    no mode has, or a depth no mode has, is an error that names what the modes have. Nothing
//!    falls back to another depth silently.
//! 2. A rate is answered by a mode's own numbers: its ceiling is the rate of its shortest frame
//!    (frame length over line time), and its floor is the rate of its longest frame. A rate above
//!    the ceiling is answered with the ceiling, and a rate below the floor with the floor. Each
//!    answer says so in its [`Reason`] and text.
//! 3. With a format or depth asked, the candidates are those modes. Among the candidates that can
//!    reach the rate, the smallest frame wins, then the fastest, then the lowest index. If none can
//!    reach it, the candidate with the highest ceiling runs (the lowest index wins a tie of
//!    ceilings, after the higher depth and the smaller frame).
//! 4. With neither asked, the highest bit depth among the candidates that can reach the rate
//!    wins; if the rate needs a lower depth, that depth is taken and the [`Reason`] says so. If no
//!    depth can reach the rate, the candidate with the highest ceiling runs, and the reason says
//!    the rate was answered at its ceiling.
//!
//! Frame rates are never nominal: every rate here is computed from a frame length in whole lines
//! over the mode's line time.

use std::cmp::Reverse;
use std::fmt;

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_contract_camera_sensor::{Mode, fps_milli_of, frame_length_for_fps_milli};
use styx_format::FourCc;

/// What a caller needs from a sensor mode.
///
/// A mode is a candidate when it is at least this big and matches each field that is set. The
/// rate is answered by the chosen mode's own numbers (see the module's rule): asking for more than
/// the mode can run at gives the mode's ceiling, and the [`Selection`] says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Request {
    pub width: u32,
    pub height: u32,
    /// The frame rate asked for, in thousandths of a frame a second (n frames a second is
    /// n * 1000). 0 asks for no rate: the chosen mode runs at its shortest frame.
    pub fps_milli: u32,
    /// The frame format the mode's frames have, as the packed four-character code (for example
    /// `pRAA` for 10-bit RGGB). `None` leaves the choice to the rule.
    pub format: Option<FourCc>,
    /// Bits per pixel on the link, if it matters. `None` leaves the choice to the rule.
    pub bit_depth: Option<u8>,
    /// Colour order, if it matters.
    pub bayer: Option<BayerOrder>,
}

/// Why a mode was chosen, and how the request was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Everything asked is met as asked. The rate may still differ from the request by the rounding
    /// to whole lines (see the achieved rate).
    Exact,
    /// The rate asked is above the chosen mode's ceiling. The mode runs at its shortest frame.
    RateClampedToCeiling,
    /// The rate asked is below the chosen mode's floor. The mode runs at its longest frame.
    RateClampedToFloor,
    /// No format or depth was asked, and the highest depth cannot reach the rate. A lower depth
    /// that can is taken.
    DepthLoweredForRate,
}

impl Reason {
    /// The reason as one word, for the service's status.
    pub const fn name(self) -> &'static str {
        match self {
            Reason::Exact => "exact",
            Reason::RateClampedToCeiling => "rate_clamped_to_ceiling",
            Reason::RateClampedToFloor => "rate_clamped_to_floor",
            Reason::DepthLoweredForRate => "depth_lowered_for_rate",
        }
    }
}

/// The mode chosen for a request, and what it gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The chosen mode's index in the sensor's modes.
    pub mode: u16,
    /// The chosen mode's frame format.
    pub format: FourCc,
    /// The chosen mode's bit depth.
    pub bit_depth: u8,
    /// The rate asked for, in milli-fps (0 when none was asked).
    pub requested_fps_milli: u32,
    /// The rate the mode gives, in milli-fps: its frame length in whole lines over its line time.
    pub achieved_fps_milli: u32,
    /// The chosen mode's readout span of one frame, in picoseconds (the sensor's `readout_ps`; 0
    /// when the sensor reports none). A frame ends at its start stamp plus this: [`frame_end_ns`].
    pub readout_ps: u64,
    /// Why this mode and rate.
    pub reason: Reason,
    /// The reason in a sentence, with the numbers (for example `asked 240.000 fps, the mode's
    /// shortest frame gives 144.910: answered at the ceiling of mode 1 (...)`).
    pub text: String,
}

/// Why no mode was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeError {
    /// The sensor reports no modes.
    NoModes,
    /// No mode is at least the asked size and matches the asked colour order, format and depth.
    NoMode(Request),
    /// The format asked is not a format of any mode. Lists the formats the modes have.
    NoFormat {
        asked: FourCc,
        available: Vec<FourCc>,
    },
    /// The bit depth asked is not the depth of any mode. Lists the depths the modes have.
    NoDepth { asked: u8, available: Vec<u8> },
    /// The mode's colour order and bit depth have no frame format here.
    NoFourCc { bayer: BayerOrder, bit_depth: u8 },
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModes => write!(f, "the sensor reports no modes"),
            Self::NoMode(r) => {
                write!(f, "no sensor mode is at least {}x{}", r.width, r.height)?;
                if let Some(format) = r.format {
                    write!(f, ", format {format}")?;
                }
                if let Some(bits) = r.bit_depth {
                    write!(f, ", {bits} bits")?;
                }
                if let Some(bayer) = r.bayer {
                    write!(f, ", {bayer:?}")?;
                }
                Ok(())
            }
            Self::NoFormat { asked, available } => write!(
                f,
                "no sensor mode has format {asked}: the modes have {}",
                list(available.iter().map(ToString::to_string))
            ),
            Self::NoDepth { asked, available } => write!(
                f,
                "no sensor mode is {asked} bits: the modes are {} bits",
                list(available.iter().map(ToString::to_string))
            ),
            Self::NoFourCc { bayer, bit_depth } => {
                write!(f, "no frame format for {bayer:?} at {bit_depth} bits")
            }
        }
    }
}

fn list(items: impl Iterator<Item = String>) -> String {
    let joined: Vec<String> = items.collect();
    if joined.is_empty() {
        "none".to_string()
    } else {
        joined.join(", ")
    }
}

impl std::error::Error for ModeError {}

/// The fastest rate a mode runs at, in thousandths of a frame per second: its shortest frame
/// (`frame_length_min` lines) over its line time, by the contract's `fps_milli_of`. `0` for a mode
/// with no line time or no frame.
pub fn fastest_fps_milli(mode: &Mode) -> u32 {
    fps_milli_of(mode.line_time_ps, mode.frame_length_min).unwrap_or(0)
}

/// The rate a mode gives for `asked` (milli-fps), and the reason it differs from the ask, if it
/// does. Asking for 0 gives the fastest rate with no reason. A rate above the ceiling gives the
/// ceiling; below the floor (the longest frame) gives the floor. Otherwise the frame length is the
/// contract's `frame_length_for_fps_milli` within the mode's limits, and the rate is that frame's.
fn rate_for(mode: &Mode, asked: u32) -> (u32, Option<Reason>) {
    let ceiling = fastest_fps_milli(mode);
    if asked == 0 {
        return (ceiling, None);
    }
    if asked > ceiling {
        return (ceiling, Some(Reason::RateClampedToCeiling));
    }
    let min = mode.frame_length_min;
    // A mode that gives no longest frame has no floor.
    let max = if mode.frame_length_max == 0 {
        None
    } else {
        Some(mode.frame_length_max.max(min))
    };
    if let Some(max) = max {
        let floor = fps_milli_of(mode.line_time_ps, max).unwrap_or(0);
        if asked < floor {
            return (floor, Some(Reason::RateClampedToFloor));
        }
    }
    match frame_length_for_fps_milli(mode.line_time_ps, min, max.unwrap_or(u32::MAX), asked) {
        Some((_lines, rate)) => (rate, None),
        // Unreachable for an asked rate under the ceiling (the ceiling needs a line time and a
        // frame), kept total rather than panicking.
        None => (ceiling, None),
    }
}

fn area(mode: &Mode) -> u64 {
    u64::from(mode.link.width) * u64::from(mode.link.height)
}

fn is_big_enough(mode: &Mode, request: &Request) -> bool {
    mode.link.width >= request.width
        && mode.link.height >= request.height
        && request.bayer.is_none_or(|b| mode.bayer == b)
}

fn matches_format_and_depth(mode: &Mode, request: &Request) -> bool {
    request
        .format
        .is_none_or(|format| fourcc_for(mode).ok() == Some(format))
        && request
            .bit_depth
            .is_none_or(|bits| mode.link.bit_depth == bits)
}

/// The formats the modes have, in mode order, each once.
fn formats(modes: &[Mode]) -> Vec<FourCc> {
    let mut out: Vec<FourCc> = Vec::new();
    for code in modes.iter().filter_map(|m| fourcc_for(m).ok()) {
        if !out.contains(&code) {
            out.push(code);
        }
    }
    out
}

/// The bit depths given, highest first, each once.
fn depths(depths: impl Iterator<Item = u8>) -> Vec<u8> {
    let mut out: Vec<u8> = depths.collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.dedup();
    out
}

/// Among `candidates`, the smallest that reaches `fps_milli`, then the fastest, then the lowest
/// index. `None` when none reaches it.
fn smallest_reaching(modes: &[Mode], candidates: &[usize], fps_milli: u32) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .filter(|&i| fastest_fps_milli(&modes[i]) >= fps_milli)
        .min_by_key(|&i| (area(&modes[i]), Reverse(fastest_fps_milli(&modes[i])), i))
}

/// Among `candidates`, the one with the highest ceiling, then the higher depth, then the smaller
/// frame, then the lowest index.
fn highest_ceiling(modes: &[Mode], candidates: &[usize]) -> Option<usize> {
    candidates.iter().copied().min_by_key(|&i| {
        (
            Reverse(fastest_fps_milli(&modes[i])),
            Reverse(modes[i].link.bit_depth),
            area(&modes[i]),
            i,
        )
    })
}

/// The index of the mode to use for `request`, and what it gives. The rule is in the module docs.
pub fn select_mode(modes: &[Mode], request: &Request) -> Result<Selection, ModeError> {
    if modes.is_empty() {
        return Err(ModeError::NoModes);
    }
    if let Some(asked) = request.format
        && !modes.iter().any(|m| fourcc_for(m).ok() == Some(asked))
    {
        return Err(ModeError::NoFormat {
            asked,
            available: formats(modes),
        });
    }
    if let Some(asked) = request.bit_depth
        && !modes.iter().any(|m| m.link.bit_depth == asked)
    {
        return Err(ModeError::NoDepth {
            asked,
            available: depths(modes.iter().map(|m| m.link.bit_depth)),
        });
    }
    let candidates: Vec<usize> = (0..modes.len())
        .filter(|&i| {
            is_big_enough(&modes[i], request) && matches_format_and_depth(&modes[i], request)
        })
        .collect();
    let no_mode = ModeError::NoMode(*request);
    if candidates.is_empty() {
        return Err(no_mode);
    }

    let explicit = request.format.is_some() || request.bit_depth.is_some();
    let (chosen, depth_lowered) = if explicit {
        let chosen = smallest_reaching(modes, &candidates, request.fps_milli)
            .or_else(|| highest_ceiling(modes, &candidates))
            .ok_or_else(|| no_mode.clone())?;
        (chosen, false)
    } else {
        let all = depths(candidates.iter().map(|&i| modes[i].link.bit_depth));
        let top = all.first().copied().unwrap_or(0);
        let mut found = None;
        for depth in all {
            let at: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&i| modes[i].link.bit_depth == depth)
                .collect();
            if let Some(i) = smallest_reaching(modes, &at, request.fps_milli) {
                found = Some((i, depth != top));
                break;
            }
        }
        match found {
            Some(found) => found,
            None => (
                highest_ceiling(modes, &candidates).ok_or_else(|| no_mode.clone())?,
                false,
            ),
        }
    };

    let mode = &modes[chosen];
    let format = fourcc_for(mode)?;
    let (achieved, clamp) = rate_for(mode, request.fps_milli);
    let reason = clamp.unwrap_or(if depth_lowered {
        Reason::DepthLoweredForRate
    } else {
        Reason::Exact
    });
    let index = u16::try_from(chosen).map_err(|_| ModeError::NoModes)?;
    let top_depth = depths(candidates.iter().map(|&i| modes[i].link.bit_depth))
        .first()
        .copied()
        .unwrap_or(mode.link.bit_depth);
    let text = describe(&Described {
        index,
        mode,
        format,
        asked: request.fps_milli,
        achieved,
        reason,
        top_depth,
    });
    Ok(Selection {
        mode: index,
        format,
        bit_depth: mode.link.bit_depth,
        requested_fps_milli: request.fps_milli,
        achieved_fps_milli: achieved,
        readout_ps: mode.readout_ps,
        reason,
        text,
    })
}

/// A frame's end time on `CLOCK_MONOTONIC`, from its start stamp and the readout span of its mode
/// (`readout_ps`, picoseconds): the start plus the span, rounded up to a whole nanosecond. Zero
/// readout ends the frame at its start. The rate arithmetic does not use this.
pub fn frame_end_ns(start_ns: u64, readout_ps: u64) -> u64 {
    start_ns.saturating_add(readout_ps.div_ceil(1_000))
}

struct Described<'a> {
    index: u16,
    mode: &'a Mode,
    format: FourCc,
    asked: u32,
    achieved: u32,
    reason: Reason,
    top_depth: u8,
}

fn fps(milli: u32) -> String {
    format!("{}.{:03}", milli / 1000, milli % 1000)
}

fn describe(d: &Described<'_>) -> String {
    let at = format!(
        "mode {} ({}x{} {}, {} bits)",
        d.index, d.mode.link.width, d.mode.link.height, d.format, d.mode.link.bit_depth
    );
    let achieved = fps(d.achieved);
    match d.reason {
        Reason::Exact if d.asked == 0 => format!("{at} runs at {achieved} fps, no rate asked"),
        Reason::Exact => format!("{at} runs at {achieved} fps, asked {}", fps(d.asked)),
        Reason::RateClampedToCeiling => format!(
            "asked {} fps, the mode's shortest frame gives {achieved}: answered at the ceiling of {at}",
            fps(d.asked)
        ),
        Reason::RateClampedToFloor => format!(
            "asked {} fps, the mode's longest frame gives {achieved}: answered at the floor of {at}",
            fps(d.asked)
        ),
        Reason::DepthLoweredForRate => format!(
            "asked {} fps: {}-bit cannot reach it, so {at} runs at {achieved} fps",
            fps(d.asked),
            d.top_depth
        ),
    }
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

    /// A mode of `width` x `height` at `bits`, with the given line time and shortest frame. Its
    /// longest frame is 52 000 lines, as on the board.
    fn board_mode(width: u32, height: u32, bits: u8, line_time_ps: u32, frame_min: u32) -> Mode {
        Mode {
            link: LinkConfig {
                width,
                height,
                bit_depth: bits,
                ..LinkConfig::default()
            },
            bayer: BayerOrder::Rggb,
            line_time_ps,
            frame_length_min: frame_min,
            frame_length_max: 52_000,
            ..Mode::default()
        }
    }

    /// The board's two 1280x800 modes: mode 0 is 10-bit (its shortest frame gives about 114.8 fps
    /// with a gated clock), mode 1 is 8-bit (about 144.8 fps).
    fn board() -> Vec<Mode> {
        vec![
            board_mode(1280, 800, 10, 8_710_801, 1000),
            board_mode(1280, 800, 8, 8_710_801, 793),
        ]
    }

    /// A mode whose shortest frame gives `fps` frames a second at 9.1 us a line, to the whole line
    /// the contract's `frame_length_for_fps_milli` picks.
    fn mode(width: u32, height: u32, fps: u32, bayer: BayerOrder, bits: u8) -> Mode {
        let line_time_ps = 9_100_000u32;
        let (frame_length_min, _) =
            frame_length_for_fps_milli(line_time_ps, 1, u32::MAX, fps * 1_000).unwrap();
        Mode {
            link: LinkConfig {
                width,
                height,
                bit_depth: bits,
                ..LinkConfig::default()
            },
            bayer,
            line_time_ps,
            frame_length_min,
            frame_length_max: 52_000,
            ..Mode::default()
        }
    }

    fn fps(milli: u32) -> Request {
        Request {
            width: 1280,
            height: 800,
            fps_milli: milli,
            ..Request::default()
        }
    }

    #[test]
    fn a_frame_ends_its_readout_after_its_start_rounded_up_to_a_nanosecond() {
        // 1 ns is 1000 ps; 1500 ps rounds up to 2 ns; no readout ends at the start.
        assert_eq!(frame_end_ns(10_000, 0), 10_000);
        assert_eq!(frame_end_ns(10_000, 1_000), 10_001);
        assert_eq!(frame_end_ns(10_000, 1_500), 10_002);
    }

    #[test]
    fn the_fastest_rate_is_the_shortest_frame_over_the_line_time() {
        // 1832 lines of 9.1 us: the contract's rate for that frame, not a nominal rate.
        let m = Mode {
            line_time_ps: 9_100_000,
            frame_length_min: 1832,
            ..Mode::default()
        };
        assert_eq!(
            fastest_fps_milli(&m),
            fps_milli_of(9_100_000, 1832).unwrap()
        );
        assert_eq!(fastest_fps_milli(&Mode::default()), 0);
    }

    #[test]
    fn the_board_modes_have_their_ceilings_from_their_shortest_frames() {
        let modes = board();
        let slow = fastest_fps_milli(&modes[0]);
        let fast = fastest_fps_milli(&modes[1]);
        assert!((114_700..114_900).contains(&slow), "10-bit {slow}");
        assert!((144_700..144_850).contains(&fast), "8-bit {fast}");
    }

    #[test]
    fn asking_for_10_bit_gets_mode_0_and_8_bit_gets_mode_1() {
        let ten = Request {
            format: Some(FourCc::RGGB10P),
            ..fps(60_000)
        };
        let chosen = select_mode(&board(), &ten).expect("10-bit");
        assert_eq!((chosen.mode, chosen.bit_depth), (0, 10));
        assert_eq!(chosen.format, FourCc::RGGB10P);
        assert_eq!(chosen.reason, Reason::Exact);

        let eight = Request {
            bit_depth: Some(8),
            ..fps(60_000)
        };
        let chosen = select_mode(&board(), &eight).expect("8-bit");
        assert_eq!((chosen.mode, chosen.bit_depth), (1, 8));
        assert_eq!(chosen.format, FourCc::RGGB);
        assert_eq!(chosen.reason, Reason::Exact);
    }

    #[test]
    fn with_no_format_and_60_fps_the_highest_depth_that_reaches_the_rate_wins() {
        // Both modes reach 60 fps: the choice is the highest bit depth, so mode 0.
        let chosen = select_mode(&board(), &fps(60_000)).expect("a mode");
        assert_eq!((chosen.mode, chosen.bit_depth), (0, 10));
        assert_eq!(chosen.reason, Reason::Exact);
        assert_eq!(chosen.requested_fps_milli, 60_000);
        // 60 fps is a whole-line frame of mode 0: 1913 lines gives about 60.0 fps.
        assert!(
            (59_900..=60_100).contains(&chosen.achieved_fps_milli),
            "{}",
            chosen.text
        );
        assert!(chosen.text.contains("asked 60.000"), "{}", chosen.text);
    }

    #[test]
    fn ten_bit_at_130_fps_is_answered_at_mode_0s_ceiling_with_a_reason() {
        let request = Request {
            format: Some(FourCc::RGGB10P),
            ..fps(130_000)
        };
        let chosen = select_mode(&board(), &request).expect("answered");
        let ceiling = fastest_fps_milli(&board()[0]);
        assert_eq!(chosen.mode, 0);
        assert_eq!(chosen.reason, Reason::RateClampedToCeiling);
        assert_eq!(chosen.achieved_fps_milli, ceiling);
        assert_eq!(chosen.requested_fps_milli, 130_000);
        assert!(
            chosen.text.contains("asked 130.000") && chosen.text.contains("mode 0"),
            "{}",
            chosen.text
        );
    }

    #[test]
    fn with_no_format_130_fps_takes_8_bit_because_10_bit_cannot_reach_it() {
        let chosen = select_mode(&board(), &fps(130_000)).expect("a lower depth");
        assert_eq!((chosen.mode, chosen.bit_depth), (1, 8));
        assert_eq!(chosen.reason, Reason::DepthLoweredForRate);
        assert!(chosen.achieved_fps_milli >= 130_000, "{}", chosen.text);
        assert!(
            chosen.text.contains("10-bit cannot reach it"),
            "{}",
            chosen.text
        );
    }

    #[test]
    fn with_no_format_240_fps_is_answered_at_the_fastest_candidates_ceiling() {
        // No depth reaches 240 fps. The fastest candidate is mode 1 (8-bit), so it runs at its
        // ceiling, and the reason says the rate was clamped.
        let chosen = select_mode(&board(), &fps(240_000)).expect("answered");
        let ceiling = fastest_fps_milli(&board()[1]);
        assert_eq!((chosen.mode, chosen.bit_depth), (1, 8));
        assert_eq!(chosen.reason, Reason::RateClampedToCeiling);
        assert_eq!(chosen.achieved_fps_milli, ceiling);
        assert!(
            chosen.text.contains("asked 240.000") && chosen.text.contains("shortest frame gives"),
            "{}",
            chosen.text
        );
    }

    #[test]
    fn a_rate_below_the_floor_is_answered_at_the_floor() {
        // The longest frame of 52 000 lines at 8.71 us is about 2.2 fps.
        let chosen = select_mode(&board(), &fps(1_000)).expect("answered");
        assert_eq!(chosen.reason, Reason::RateClampedToFloor);
        assert!(chosen.achieved_fps_milli > 2_000 && chosen.achieved_fps_milli < 2_400);
    }

    #[test]
    fn an_unknown_format_is_an_error_that_names_the_formats() {
        let request = Request {
            format: Some(FourCc::YUYV),
            ..fps(60_000)
        };
        let error = select_mode(&board(), &request).expect_err("no such format");
        assert_eq!(
            error,
            ModeError::NoFormat {
                asked: FourCc::YUYV,
                available: vec![FourCc::RGGB10P, FourCc::RGGB],
            }
        );
        let text = error.to_string();
        assert!(text.contains("pRAA") && text.contains("RGGB"), "{text}");
    }

    #[test]
    fn an_unknown_depth_is_an_error_that_names_the_depths() {
        let request = Request {
            bit_depth: Some(12),
            ..fps(60_000)
        };
        let error = select_mode(&board(), &request).expect_err("no 12-bit mode");
        assert_eq!(
            error,
            ModeError::NoDepth {
                asked: 12,
                available: vec![10, 8],
            }
        );
    }

    #[test]
    fn a_frame_larger_than_every_mode_is_no_mode() {
        let big = Request {
            width: 4000,
            height: 3000,
            ..Request::default()
        };
        assert_eq!(select_mode(&board(), &big), Err(ModeError::NoMode(big)));
        assert_eq!(select_mode(&[], &big), Err(ModeError::NoModes));
    }

    #[test]
    fn the_smallest_mode_that_reaches_the_rate_wins_and_the_fastest_of_equal_size() {
        let modes = vec![
            mode(640, 480, 60, BayerOrder::Rggb, 10),
            mode(320, 240, 120, BayerOrder::Rggb, 10),
            mode(320, 240, 30, BayerOrder::Rggb, 8),
            mode(1280, 720, 30, BayerOrder::Gbrg, 10),
        ];
        // 60 fps: the 640x480 mode's ceiling is 59.984, which does not reach it, so the 320x240
        // mode at 120 fps wins the smallest-frame rule.
        let request = Request {
            width: 320,
            height: 240,
            fps_milli: 60_000,
            ..Request::default()
        };
        assert_eq!(select_mode(&modes, &request).map(|s| s.mode), Ok(1));
        // Two 10-bit modes fit a small request with no rate: the smaller one, 320x240.
        let request = Request {
            width: 300,
            height: 200,
            ..Request::default()
        };
        assert_eq!(select_mode(&modes, &request).map(|s| s.mode), Ok(1));
        let eight_bit = Request {
            bit_depth: Some(8),
            ..request
        };
        let chosen = select_mode(&modes, &eight_bit).expect("8-bit");
        assert_eq!((chosen.mode, chosen.reason), (2, Reason::Exact));
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

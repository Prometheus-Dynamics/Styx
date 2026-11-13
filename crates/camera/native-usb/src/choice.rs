//! Which of a camera's formats to stream.

use std::cmp::Reverse;

use lemnos_contract_camera_frames::FrameFormat;
use styx_format::FourCc;

/// What the caller wants from the camera's formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preference {
    /// Uncompressed codes, best first. A format whose code is not listed is never chosen.
    pub codes: Vec<FourCc>,
    /// The frame rate, in thousandths, a format should reach to be chosen over a slower one.
    pub target_fps_milli: u32,
    /// The largest frame, in pixels, to choose. `None` for no limit.
    pub max_pixels: Option<u32>,
}

impl Default for Preference {
    fn default() -> Self {
        Self {
            codes: vec![FourCc::YUYV, FourCc::NV12, FourCc::GREY],
            target_fps_milli: 30_000,
            max_pixels: None,
        }
    }
}

/// The format chosen: its index in the camera's list, and the format itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub index: u16,
    pub format: FrameFormat,
}

/// The bytes one frame of `format` takes at its size, or `None` when it has no size this
/// crate can lay out.
fn frame_bytes(code: FourCc, width: u32, height: u32) -> Option<usize> {
    let planes = styx_format::plane_sizes(code, width, height, None, None).ok()?;
    planes
        .iter()
        .try_fold(0usize, |sum, plane| sum.checked_add(plane.len))
}

/// Chooses one of `formats` under `pref`, or `None` when none is usable.
///
/// A format is usable when its code is in `pref.codes`, it is uncompressed (compressed frames
/// have no layout this crate can map yet), its size fits `pref.max_pixels` and the camera's
/// `max_frame_bytes` holds one frame. Among usable formats the earliest code in `pref.codes`
/// wins, then a format that reaches `pref.target_fps_milli`, then the largest frame, then the
/// fastest. Ties keep the camera's own order.
pub fn choose(formats: &[FrameFormat], pref: &Preference) -> Option<Choice> {
    formats
        .iter()
        .enumerate()
        .filter_map(|(index, format)| {
            let code = FourCc::new(format.fourcc.to_le_bytes());
            let rank = pref.codes.iter().position(|c| *c == code)?;
            if code.is_compressed() || format.width == 0 || format.height == 0 {
                return None;
            }
            let pixels = u64::from(format.width) * u64::from(format.height);
            if pref.max_pixels.is_some_and(|max| pixels > u64::from(max)) {
                return None;
            }
            let bytes = frame_bytes(code, format.width, format.height)?;
            if bytes == 0 || bytes > usize::try_from(format.max_frame_bytes).ok()? {
                return None;
            }
            let misses_target = format.fps_milli < pref.target_fps_milli;
            let key = (
                rank,
                misses_target,
                Reverse(pixels),
                Reverse(format.fps_milli),
                index,
            );
            Some((
                key,
                Choice {
                    index: u16::try_from(index).ok()?,
                    format: *format,
                },
            ))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, choice)| choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(code: &[u8; 4], width: u32, height: u32, fps_milli: u32, max: u32) -> FrameFormat {
        FrameFormat {
            fourcc: u32::from_le_bytes(*code),
            width,
            height,
            fps_milli,
            max_frame_bytes: max,
        }
    }

    const YUYV_640_480_30: FrameFormat = FrameFormat {
        fourcc: u32::from_le_bytes(*b"YUYV"),
        width: 640,
        height: 480,
        fps_milli: 30_000,
        max_frame_bytes: 614_400,
    };

    #[test]
    fn largest_yuyv_at_target_rate_wins() {
        let formats = [
            fmt(b"MJPG", 1920, 1080, 30_000, 400_000),
            fmt(b"YUYV", 320, 240, 30_000, 153_600),
            fmt(b"YUYV", 640, 480, 15_000, 614_400),
            YUYV_640_480_30,
        ];
        let choice = choose(&formats, &Preference::default()).expect("a choice");
        assert_eq!(choice.index, 3);
        assert_eq!(choice.format, YUYV_640_480_30);
    }

    #[test]
    fn compressed_formats_are_never_chosen() {
        let formats = [fmt(b"MJPG", 1280, 720, 30_000, 300_000)];
        assert_eq!(choose(&formats, &Preference::default()), None);
    }

    #[test]
    fn a_code_outside_the_preference_is_skipped() {
        let formats = [fmt(b"GREY", 64, 64, 30_000, 4096)];
        let pref = Preference {
            codes: vec![FourCc::YUYV],
            ..Preference::default()
        };
        assert_eq!(choose(&formats, &pref), None);
    }

    #[test]
    fn earlier_code_beats_a_larger_frame() {
        let formats = [
            fmt(b"NV12", 1280, 720, 30_000, 1_382_400),
            fmt(b"YUYV", 320, 240, 30_000, 153_600),
        ];
        let choice = choose(&formats, &Preference::default()).expect("a choice");
        assert_eq!(choice.index, 1);
    }

    #[test]
    fn when_no_format_reaches_the_target_the_fastest_largest_one_wins() {
        let formats = [
            fmt(b"YUYV", 640, 480, 15_000, 614_400),
            fmt(b"YUYV", 320, 240, 20_000, 153_600),
        ];
        let choice = choose(&formats, &Preference::default()).expect("a choice");
        assert_eq!(choice.index, 0);
    }

    #[test]
    fn max_pixels_limits_the_choice() {
        let formats = [YUYV_640_480_30, fmt(b"YUYV", 320, 240, 30_000, 153_600)];
        let pref = Preference {
            max_pixels: Some(320 * 240),
            ..Preference::default()
        };
        assert_eq!(choose(&formats, &pref).map(|c| c.index), Some(1));
    }

    #[test]
    fn a_frame_size_the_camera_cannot_hold_is_skipped() {
        let formats = [fmt(b"YUYV", 640, 480, 30_000, 1000)];
        assert_eq!(choose(&formats, &Preference::default()), None);
    }
}

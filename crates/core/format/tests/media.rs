//! Sizes, intervals and colour.

use styx_format::{
    ColorRange, ColorSpace, FourCc, Interval, IntervalStepwise, MediaFormat, Resolution,
};

#[test]
fn zero_sizes_and_rates_are_refused() {
    assert!(Resolution::new(0, 480).is_none());
    assert!(Interval::from_fps(0).is_none());
    assert_eq!(Interval::from_fps(30).unwrap().fps(), 30.0);
}

#[test]
fn stepwise_intervals_take_whole_rate_steps() {
    let range = IntervalStepwise {
        min: Interval::from_fps(60).unwrap(),
        max: Interval::from_fps(30).unwrap(),
        step: Interval::from_fps(10).unwrap(),
    };
    assert!(range.contains(Interval::from_fps(60).unwrap()));
    assert!(range.contains(Interval::from_fps(40).unwrap()));
    assert!(range.contains(Interval::from_fps(30).unwrap()));
    assert!(!range.contains(Interval::from_fps(45).unwrap()));
    assert!(!range.contains(Interval::from_fps(20).unwrap()));
    assert!(!range.contains(Interval::from_fps(120).unwrap()));
}

#[test]
fn colour_codes_round_trip() {
    for c in [
        ColorSpace::Srgb,
        ColorSpace::Bt709,
        ColorSpace::Bt2020,
        ColorSpace::Bt601,
        ColorSpace::Unknown,
    ] {
        assert_eq!(ColorSpace::from_code(c.code()), c);
    }
}

#[test]
fn formats_take_their_defaults() {
    let size = Resolution::new(640, 480).unwrap();
    let nv12 = MediaFormat::with_defaults(FourCc::NV12, size);
    assert_eq!(
        (nv12.color, nv12.range),
        (ColorSpace::Bt709, ColorRange::Limited)
    );
    let rgb = MediaFormat::with_defaults(FourCc::RG24, size);
    assert_eq!((rgb.color, rgb.range), (ColorSpace::Srgb, ColorRange::Full));
    let raw = MediaFormat::with_defaults(FourCc::BG10, size);
    assert_eq!(
        (raw.color, raw.range),
        (ColorSpace::Unknown, ColorRange::Full)
    );
    #[cfg(feature = "serde")]
    {
        let json = serde_json::to_string(&nv12).unwrap();
        assert_eq!(serde_json::from_str::<MediaFormat>(&json).unwrap(), nv12);
    }
}

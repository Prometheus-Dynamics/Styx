//! Each control's range, default and automatic mode, and what `validate` takes.

use styx_controls::{Control, ControlValue, Range, Rect, ValueError};

#[test]
fn the_set_has_eight_controls_in_index_order() {
    assert_eq!(Control::ALL.len(), Control::COUNT);
    for (i, control) in Control::ALL.iter().enumerate() {
        assert_eq!(control.index(), i);
        assert_eq!(control.spec().control, *control);
    }
    let mut names: Vec<&str> = Control::ALL.iter().map(|c| c.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), Control::COUNT, "names must be unique");
}

#[test]
fn every_default_is_valid_for_its_control() {
    for control in Control::ALL {
        let default = control.spec().default;
        assert_eq!(control.validate(default), Ok(()), "{}", control.name());
    }
}

#[test]
fn the_documented_defaults() {
    assert_eq!(Control::Exposure.spec().default, ControlValue::Auto);
    assert_eq!(Control::Gain.spec().default, ControlValue::Auto);
    assert_eq!(Control::WhiteBalance.spec().default, ControlValue::Auto);
    assert_eq!(Control::Focus.spec().default, ControlValue::Auto);
    assert_eq!(
        Control::FrameRate.spec().default,
        ControlValue::Manual(30_000)
    );
    assert_eq!(Control::HFlip.spec().default, ControlValue::Flag(false));
    assert_eq!(Control::VFlip.spec().default, ControlValue::Flag(false));
    assert_eq!(Control::Crop.spec().default, ControlValue::Crop(None));
}

#[test]
fn manual_ranges_accept_both_ends_and_refuse_outside() {
    for control in Control::ALL {
        let Range::Manual { min, max } = control.spec().range else {
            continue;
        };
        assert!(min < max, "{}", control.name());
        assert_eq!(control.validate(ControlValue::Manual(min)), Ok(()));
        assert_eq!(control.validate(ControlValue::Manual(max)), Ok(()));
        if let Some(below) = min.checked_sub(1) {
            assert_eq!(
                control.validate(ControlValue::Manual(below)),
                Err(ValueError::OutOfRange {
                    min,
                    max,
                    value: below
                }),
                "{}",
                control.name()
            );
        }
        assert!(control.validate(ControlValue::Manual(max + 1)).is_err());
    }
}

#[test]
fn exposure_range_is_one_microsecond_to_one_second() {
    assert_eq!(
        Control::Exposure.spec().range,
        Range::Manual {
            min: 1,
            max: 1_000_000
        }
    );
    assert!(Control::Exposure.validate(ControlValue::Manual(0)).is_err());
}

#[test]
fn gain_range_is_unity_to_sixteen_times() {
    assert_eq!(
        Control::Gain.spec().range,
        Range::Manual {
            min: 1_000,
            max: 16_000
        }
    );
    assert!(Control::Gain.validate(ControlValue::Manual(999)).is_err());
}

#[test]
fn focus_range_is_normalised_zero_to_one_thousand() {
    assert_eq!(
        Control::Focus.spec().range,
        Range::Manual { min: 0, max: 1_000 }
    );
}

#[test]
fn auto_is_accepted_only_where_the_control_has_an_automatic_mode() {
    for control in Control::ALL {
        let auto = control.spec().auto;
        let result = control.validate(ControlValue::Auto);
        if auto {
            assert_eq!(result, Ok(()), "{}", control.name());
        } else {
            assert_eq!(result, Err(ValueError::NoAuto), "{}", control.name());
        }
    }
    assert!(Control::Exposure.spec().auto);
    assert!(!Control::FrameRate.spec().auto);
    assert!(!Control::Crop.spec().auto);
}

#[test]
fn a_value_of_the_wrong_kind_is_refused() {
    assert_eq!(
        Control::Exposure.validate(ControlValue::Flag(true)),
        Err(ValueError::WrongKind)
    );
    assert_eq!(
        Control::HFlip.validate(ControlValue::Manual(1)),
        Err(ValueError::WrongKind)
    );
    assert_eq!(
        Control::Crop.validate(ControlValue::Manual(1)),
        Err(ValueError::WrongKind)
    );
}

#[test]
fn flips_take_either_flag() {
    for control in [Control::HFlip, Control::VFlip] {
        assert_eq!(control.validate(ControlValue::Flag(true)), Ok(()));
        assert_eq!(control.validate(ControlValue::Flag(false)), Ok(()));
    }
}

#[test]
fn crop_takes_the_whole_array_or_a_non_empty_rectangle() {
    let rect = Rect {
        x: 4,
        y: 8,
        width: 640,
        height: 480,
    };
    assert_eq!(Control::Crop.validate(ControlValue::Crop(None)), Ok(()));
    assert_eq!(
        Control::Crop.validate(ControlValue::Crop(Some(rect))),
        Ok(())
    );
    let empty_width = Rect { width: 0, ..rect };
    assert_eq!(
        Control::Crop.validate(ControlValue::Crop(Some(empty_width))),
        Err(ValueError::EmptyCrop)
    );
    let empty_height = Rect { height: 0, ..rect };
    assert_eq!(
        Control::Crop.validate(ControlValue::Crop(Some(empty_height))),
        Err(ValueError::EmptyCrop)
    );
}

#[test]
fn value_errors_display_their_reason() {
    let text = ValueError::OutOfRange {
        min: 1,
        max: 9,
        value: 12,
    }
    .to_string();
    assert_eq!(text, "value 12 is outside 1..=9");
}

#[cfg(feature = "std")]
#[test]
fn value_error_is_an_error() {
    fn takes_error(_: &dyn std::error::Error) {}
    takes_error(&ValueError::NoAuto);
}

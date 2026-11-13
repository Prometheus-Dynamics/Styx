//! The planner: which outputs serve which consumers, and the refusals.

mod common;

use common::*;
use styx_format::{FourCc, MediaFormat};
use styx_outputs::{Ask, PISP, PlanError, Route, plan};
use styx_settings::IspSettings;

fn fmt(code: FourCc, w: u32, h: u32) -> MediaFormat {
    MediaFormat::with_defaults(code, res(w, h))
}

#[test]
fn two_sizes_from_one_capture_use_both_outputs_and_identical_asks_share() {
    let asks = [
        Ask::new(FourCc::RGB3, res(64, 48)),
        Ask::new(FourCc::RGB3, res(32, 24)),
        Ask::new(FourCc::RGB3, res(64, 48)),
    ];
    let plan = plan(&Soft, capture(), &asks).unwrap();

    assert_eq!(plan.outputs().len(), 2);
    assert_eq!(plan.outputs()[0].format, fmt(FourCc::RGB3, 64, 48));
    assert_eq!(plan.outputs()[1].format, fmt(FourCc::RGB3, 32, 24));
    assert!(plan.outputs().iter().all(|o| o.used));
    assert!(plan.conversions().is_empty());
    assert_eq!(
        plan.routes(),
        &[
            Route::Isp {
                output: 0,
                shared: true
            },
            Route::Isp {
                output: 1,
                shared: false
            },
            Route::Isp {
                output: 0,
                shared: true
            },
        ]
    );

    let mut settings = IspSettings::neutral(0.0);
    plan.apply(&mut settings);
    assert_eq!(
        settings.outputs,
        plan.outputs().iter().map(|o| o.format).collect::<Vec<_>>()
    );
}

#[test]
fn one_size_in_a_family_sibling_format_takes_the_one_conversion() {
    let asks = [
        Ask::new(FourCc::NV12, res(64, 48)),
        Ask::new(FourCc::YU12, res(64, 48)),
    ];
    let plan = plan(&Soft, capture(), &asks).unwrap();

    assert_eq!(plan.outputs().len(), 1);
    assert_eq!(plan.conversions().len(), 1);
    assert_eq!(plan.conversions()[0].from, 0);
    assert_eq!(plan.conversions()[0].format, fmt(FourCc::YU12, 64, 48));
    assert_eq!(
        plan.routes(),
        &[
            Route::Isp {
                output: 0,
                shared: false
            },
            Route::Converted {
                conversion: 0,
                shared: false
            },
        ]
    );
}

#[test]
fn consumers_sharing_a_converted_format_share_the_conversion() {
    let asks = [
        Ask::new(FourCc::NV12, res(64, 48)),
        Ask::new(FourCc::YU12, res(64, 48)),
        Ask::new(FourCc::YU12, res(64, 48)),
    ];
    let plan = plan(&Soft, capture(), &asks).unwrap();
    assert_eq!(plan.conversions().len(), 1);
    assert_eq!(plan.route(1), plan.route(2));
    assert_eq!(
        plan.route(2),
        Some(Route::Converted {
            conversion: 0,
            shared: true
        })
    );
}

#[test]
fn a_size_no_output_makes_is_refused_with_the_reason() {
    let err = plan(&Soft, capture(), &[Ask::new(FourCc::NV12, res(16, 12))]).unwrap_err();
    let PlanError::Unserved {
        consumer,
        ask,
        reason,
    } = err
    else {
        panic!("expected a refusal, got {err:?}");
    };
    assert_eq!(consumer, 0);
    assert_eq!(ask, Ask::new(FourCc::NV12, res(16, 12)));
    assert!(reason.contains("soft cannot make 16x12 NV12"), "{reason}");
    assert!(reason.contains("output 0 makes full size only"), "{reason}");
    assert!(
        reason.contains("output 1 makes 1/2 size only (32x24)"),
        "{reason}"
    );
}

#[test]
fn a_size_above_the_capture_is_refused_as_an_upscale() {
    let err = plan(&Soft, capture(), &[Ask::new(FourCc::RGB3, res(128, 96))]).unwrap_err();
    assert_eq!(
        err,
        PlanError::Upscale {
            consumer: 0,
            ask: Ask::new(FourCc::RGB3, res(128, 96)),
            capture: capture(),
        }
    );
    assert!(err.to_string().contains("no ISP output upscales"));
}

#[test]
fn a_colour_matrix_conversion_is_refused_not_faked() {
    // RGB3 at full size is output 0; NV12 at the same size would need a colour matrix, which
    // styx-scale does not convert, and output 1 is half size only.
    let asks = [
        Ask::new(FourCc::RGB3, res(64, 48)),
        Ask::new(FourCc::NV12, res(64, 48)),
    ];
    let err = plan(&Soft, capture(), &asks).unwrap_err();
    let PlanError::Unserved {
        consumer, reason, ..
    } = err
    else {
        panic!("expected a refusal");
    };
    assert_eq!(consumer, 1);
    assert!(
        reason.contains("soft cannot make 64x48 NV12 beside the other asks"),
        "{reason}"
    );
}

#[test]
fn three_distinct_asks_in_two_outputs_are_refused_together() {
    // NV12 full and NV12 half take both outputs; RGB3 half has no output left and no conversion
    // from NV12 to RGB3 at half size.
    let asks = [
        Ask::new(FourCc::NV12, res(64, 48)),
        Ask::new(FourCc::NV12, res(32, 24)),
        Ask::new(FourCc::RGB3, res(32, 24)),
    ];
    let err = plan(&Soft, capture(), &asks).unwrap_err();
    let PlanError::Unserved {
        consumer, reason, ..
    } = err
    else {
        panic!("expected a refusal");
    };
    assert_eq!(consumer, 2);
    assert!(
        reason.contains("soft cannot make 32x24 RGB3 beside the other asks"),
        "{reason}"
    );
}

#[test]
fn an_isp_with_no_exposed_formats_refuses_every_format() {
    let err = plan(&PISP, capture(), &[Ask::new(FourCc::NV12, res(64, 48))]).unwrap_err();
    let PlanError::Unserved { reason, .. } = err else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("no format exposed"), "{reason}");
}

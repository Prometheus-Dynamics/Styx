//! Interpolation between two tunings (as between two colour temperatures): the endpoints, the
//! blended scalars and tables, the tone curve over the union of breakpoints, and the fields that
//! take the nearer side whole.

mod common;

use styx_aaa::{LensShading, Pwl, SharpenParams, ZoneGrid};
use styx_format::{FourCc, MediaFormat};
use styx_settings::IspSettings;

fn near(x: f64, y: f64) {
    assert!((x - y).abs() < 1e-12, "{x} != {y}");
}

/// A warm tuning and a cool one: white balance and matrix differ, everything else the same.
fn warm_and_cool() -> (IspSettings, IspSettings) {
    let mut warm = IspSettings::neutral(0.0625);
    warm.from_frame = 10;
    warm.wb = [1.0, 1.0, 2.0];
    warm.ccm = [1.25, -0.25, 0.0, 0.0, 1.0, 0.0, 0.0, -0.5, 1.5];
    let mut cool = warm.clone();
    cool.from_frame = 20;
    cool.wb = [2.0, 1.0, 1.0];
    cool.ccm = [1.0, 0.0, 0.0, 0.0, 1.5, -0.5, -0.25, 0.0, 1.25];
    (warm, cool)
}

#[test]
fn the_endpoints_are_the_inputs() {
    let a = common::rich();
    let mut b = a.clone();
    b.wb[1] = 0.5;
    b.gamma = Some(Pwl::constant(0.3));
    assert_eq!(a.interpolate(&b, 0.0), a);
    assert_eq!(a.interpolate(&b, 1.0), b);
    // Outside [0, 1] the weight is clamped, and NaN counts as 0.
    assert_eq!(a.interpolate(&b, -3.0), a);
    assert_eq!(a.interpolate(&b, 7.0), b);
    assert_eq!(a.interpolate(&b, f64::NAN), a);
}

#[test]
fn scalars_gains_and_matrices_blend_linearly() {
    let (warm, cool) = warm_and_cool();
    let mid = warm.interpolate(&cool, 0.25);
    // (1 - t) * warm + t * cool, with t = 0.25
    for (got, want) in mid.wb.iter().zip([1.25, 1.0, 1.75]) {
        near(*got, want);
    }
    for (got, want) in mid.ccm.iter().zip([
        1.1875, -0.1875, 0.0, 0.0, 1.125, -0.125, -0.0625, -0.375, 1.4375,
    ]) {
        near(*got, want);
    }
    near(mid.black_level, 0.0625);
    near(mid.digital_gain, 1.0);
    let half = warm.interpolate(&cool, 0.5);
    near(half.wb[0], 1.5);
    near(half.wb[2], 1.5);
}

#[test]
fn an_interpolated_setting_is_itself_a_valid_settings_value() {
    let (warm, cool) = warm_and_cool();
    let mid = warm.interpolate(&cool, 0.3);
    let text = serde_json::to_string(&mid).unwrap();
    let back: IspSettings = serde_json::from_str(&text).unwrap();
    assert_eq!(back, mid);
    // Blending a value with itself gives itself.
    assert_eq!(mid.interpolate(&mid, 0.6), mid);
}

#[test]
fn tables_blend_entry_by_entry_when_their_shapes_match() {
    let a = common::rich();
    let mut b = a.clone();
    {
        let ls = b.lens_shading.as_mut().unwrap();
        ls.r = vec![2.0, 2.0, 2.0, 2.0];
        ls.generation = 0;
    }
    b.histogram_weights = Some(ZoneGrid {
        width: 2,
        height: 2,
        zones: vec![4.0, 4.0, 4.0, 4.0],
    });
    let mid = a.interpolate(&b, 0.5);
    let ls = mid.lens_shading.as_ref().unwrap();
    for (got, want) in ls.r.iter().zip([1.5, 1.5625, 1.625, 1.75]) {
        near(*got, want);
    }
    assert_eq!(
        ls.generation, 0,
        "a blended table has no generation of its inputs"
    );
    for (got, want) in mid
        .histogram_weights
        .as_ref()
        .unwrap()
        .zones
        .iter()
        .zip([2.0, 2.5, 3.0, 2.25])
    {
        near(*got, want);
    }
}

#[test]
fn a_table_of_another_shape_takes_the_nearer_side_whole() {
    let a = common::rich();
    let mut b = a.clone();
    b.lens_shading = Some(LensShading {
        width: 1,
        height: 1,
        r: vec![3.0],
        g: vec![3.0],
        b: vec![3.0],
        generation: 0,
    });
    assert_eq!(a.interpolate(&b, 0.4).lens_shading, a.lens_shading);
    assert_eq!(a.interpolate(&b, 0.6).lens_shading, b.lens_shading);
}

#[test]
fn the_tone_curve_blends_over_the_union_of_breakpoints() {
    let mut a = IspSettings::neutral(0.0);
    a.gamma = Some(Pwl::new(vec![(0.0, 0.0), (1.0, 1.0)]).unwrap());
    let mut b = a.clone();
    b.gamma = Some(Pwl::new(vec![(0.0, 0.0), (0.5, 0.8), (1.0, 1.0)]).unwrap());
    let mid = a.interpolate(&b, 0.5);
    let points = mid.gamma.as_ref().unwrap().points();
    assert_eq!(points.len(), 3);
    for ((x, y), (wx, wy)) in points.iter().zip([(0.0, 0.0), (0.5, 0.65), (1.0, 1.0)]) {
        near(*x, wx);
        near(*y, wy);
    }
    // Between the knots the blend is still the blend of the two curves.
    let g = mid.gamma.as_ref().unwrap();
    near(
        g.eval(0.25),
        0.5 * a.gamma.as_ref().unwrap().eval(0.25) + 0.5 * b.gamma.as_ref().unwrap().eval(0.25),
    );
}

#[test]
fn an_option_set_on_one_side_only_takes_the_nearer_side_whole() {
    let a = common::rich();
    let mut b = a.clone();
    b.sharpen = None;
    b.denoise.cdn = None;
    b.gamma = None;
    assert_eq!(a.interpolate(&b, 0.25).sharpen, a.sharpen);
    assert_eq!(a.interpolate(&b, 0.75).sharpen, None);
    assert_eq!(a.interpolate(&b, 0.25).denoise.cdn, a.denoise.cdn);
    assert_eq!(a.interpolate(&b, 0.75).denoise.cdn, None);
    assert_eq!(a.interpolate(&b, 0.25).gamma, a.gamma);
    assert_eq!(a.interpolate(&b, 0.75).gamma, None);
}

#[test]
fn both_sides_set_an_option_block_blends_its_fields() {
    let a = common::rich();
    let mut b = a.clone();
    b.sharpen = Some(SharpenParams {
        threshold: 0.25,
        strength: 0.25,
        limit: 0.0,
    });
    let mid = a.interpolate(&b, 0.5);
    let s = mid.sharpen.unwrap();
    near(s.threshold, 0.75);
    near(s.strength, 0.5);
    near(s.limit, 1.0);
}

#[test]
fn discrete_fields_and_outputs_are_never_blended() {
    let a = common::rich();
    let mut b = a.clone();
    b.from_frame = 100;
    b.denoise.dpc = 2;
    b.outputs = vec![MediaFormat::with_defaults(
        FourCc::GREY,
        common::res(320, 240),
    )];
    let early = a.interpolate(&b, 0.2);
    let late = a.interpolate(&b, 0.8);
    assert_eq!(early.from_frame, 42);
    assert_eq!(late.from_frame, 100);
    assert_eq!(early.denoise.dpc, 1);
    assert_eq!(late.denoise.dpc, 2);
    assert_eq!(
        early.outputs, a.outputs,
        "sizes and formats are a choice: the nearer list whole"
    );
    assert_eq!(late.outputs, b.outputs);
    // Exactly halfway counts as the later tuning, so the rule is one-sided and stated.
    assert_eq!(a.interpolate(&b, 0.5).outputs, b.outputs);
}

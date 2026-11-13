//! Comparison: equality, and the named field-wise diff.

mod common;

use styx_aaa::Pwl;
use styx_format::{FourCc, MediaFormat};

#[test]
fn equal_settings_have_no_changed_fields() {
    let a = common::rich();
    let b = a.clone();
    assert_eq!(a, b);
    assert!(a.changed_fields(&b).is_empty());
}

#[test]
fn the_generation_tag_alone_is_not_a_change() {
    // A lens shading table copied with another generation holds the same gains.
    let a = common::rich();
    let mut b = a.clone();
    b.lens_shading.as_mut().unwrap().generation = 99;
    assert_eq!(a, b);
    assert!(a.changed_fields(&b).is_empty());
}

#[test]
fn the_diff_names_each_changed_field() {
    let a = common::rich();
    let mut b = a.clone();
    b.wb[0] = 1.75;
    b.ccm[4] = 2.0;
    b.gamma = Some(Pwl::constant(0.5));
    b.lens_shading.as_mut().unwrap().r[0] = 9.0;
    b.denoise.sdn.as_mut().unwrap().strength = 0.25;
    b.sharpen = None;
    b.outputs[0].resolution = common::res(1280, 720);
    assert_ne!(a, b);
    assert_eq!(
        a.changed_fields(&b),
        [
            "wb[0]",
            "ccm[4]",
            "gamma",
            "lens_shading.r",
            "denoise.sdn.strength",
            "sharpen",
            "outputs[0].resolution",
        ]
    );
    // The diff is symmetric.
    assert_eq!(a.changed_fields(&b), b.changed_fields(&a));
}

#[test]
fn the_diff_names_scalars_options_and_list_lengths() {
    let a = common::rich();
    let mut b = a.clone();
    b.from_frame = 43;
    b.black_level = 0.5;
    b.digital_gain = 2.0;
    b.flicker_bands = Some(vec![1.0]);
    b.denoise.tdn = None;
    b.histogram_weights.as_mut().unwrap().zones[3] = 4.0;
    b.outputs.push(MediaFormat::with_defaults(
        FourCc::GREY,
        common::res(320, 240),
    ));
    assert_eq!(
        a.changed_fields(&b),
        [
            "from_frame",
            "black_level",
            "digital_gain",
            "flicker_bands.len",
            "denoise.tdn",
            "histogram_weights.zones",
            "outputs.len",
        ]
    );
}

#[test]
fn a_differing_element_inside_a_list_is_named_by_index() {
    let a = common::rich();
    let mut b = a.clone();
    b.flicker_bands.as_mut().unwrap()[2] = 0.5;
    b.black_channels.as_mut().unwrap()[1] = 0.0;
    assert_eq!(
        a.changed_fields(&b),
        ["black_channels[1]", "flicker_bands[2]"]
    );
}

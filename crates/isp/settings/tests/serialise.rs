//! Serialisation: the settings round-trip, and the controller's own JSON reads and writes here
//! unchanged.

mod common;

use styx_settings::IspSettings;

#[test]
fn settings_round_trip_through_json() {
    let s = common::rich();
    let text = serde_json::to_string(&s).unwrap();
    let back: IspSettings = serde_json::from_str(&text).unwrap();
    assert_eq!(back, s, "text round trip: {text}");

    let value = serde_json::to_value(&s).unwrap();
    let back: IspSettings = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), value);
}

#[test]
fn outputs_are_left_out_when_empty() {
    let mut s = common::rich();
    s.outputs.clear();
    let value = serde_json::to_value(&s).unwrap();
    assert!(value.get("outputs").is_none(), "{value}");
    assert!(value.get("black_channels").is_some());

    let plain = IspSettings::neutral(0.0625);
    let value = serde_json::to_value(&plain).unwrap();
    assert!(value.get("outputs").is_none());
    assert!(value.get("flicker_bands").is_none());
    assert!(
        value.get("sharpen").is_some(),
        "an unset option is written as null"
    );
}

#[test]
fn missing_optional_fields_take_their_defaults() {
    // Only the fields without a default are present: flicker, denoise, outputs and the
    // optional blocks come from their defaults.
    let text = r#"{
        "from_frame": 3, "black_level": 0.0625, "wb": [1.0, 1.0, 1.0],
        "digital_gain": 1.0, "ccm": [1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0],
        "gamma": null, "lens_shading": null, "sharpen": null
    }"#;
    let s: IspSettings = serde_json::from_str(text).unwrap();
    assert_eq!(s, {
        let mut n = IspSettings::neutral(0.0625);
        n.from_frame = 3;
        n
    });
}

#[test]
fn controller_neutral_settings_convert_and_compare_as_json() {
    let c = styx_controller::IspSettings::neutral(0.0625);
    let want = serde_json::to_value(&c).unwrap();
    let ours: IspSettings = c.into();
    assert_eq!(serde_json::to_value(&ours).unwrap(), want);
}

#[test]
fn controller_recorded_settings_parse_and_reserialise_unchanged() {
    // Every ISP settings object the controller wrote for the recorded replay (its start line and
    // each frame) must read as these settings and write back as the same JSON.
    const RECORDED: &str = include_str!("../../../algo/controller/tests/data/dev-expected.jsonl");
    let mut checked = 0;
    for line in RECORDED.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let isp = v
            .get("isp")
            .or_else(|| v.get("start").and_then(|s| s.get("isp")))
            .expect("an isp object");
        let parsed: IspSettings = serde_json::from_value(isp.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&parsed).unwrap(), isp);
        checked += 1;
    }
    assert_eq!(checked, 81, "every line of the recording carries settings");
}

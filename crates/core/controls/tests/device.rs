//! Device controls: names held inline and cut to fit, and the kinds and serialisation.

use styx_controls::{DEVICE_NAME_CAPACITY, DeviceControl, DeviceKind, DeviceName};

#[test]
fn a_name_is_held_as_given() {
    let name = DeviceName::new("Horizontal Flip");
    assert_eq!(name.as_str(), "Horizontal Flip");
    assert_eq!(name.to_string(), "Horizontal Flip");
    assert_eq!(DeviceName::default().as_str(), "");
}

#[test]
fn a_long_name_is_cut_to_capacity_on_a_character_boundary() {
    let long = "x".repeat(40);
    assert_eq!(DeviceName::new(&long).as_str().len(), DEVICE_NAME_CAPACITY);

    // 31 ASCII bytes then a two-byte character: the character would end at 33, so it is dropped.
    let text = format!("{}é", "a".repeat(31));
    let name = DeviceName::new(&text);
    assert_eq!(name.as_str(), "a".repeat(31));
}

#[test]
fn a_name_is_plain_data() {
    fn plain<T: Copy + Eq + core::hash::Hash>() {}
    plain::<DeviceName>();
    plain::<DeviceControl>();
    let a = DeviceName::new("Brightness");
    let b = a;
    assert_eq!(a, b);
}

#[test]
fn a_device_control_describes_its_bounds_and_mode() {
    let control = DeviceControl {
        id: 0x0098_0900,
        name: DeviceName::new("Brightness"),
        kind: DeviceKind::Integer,
        min: 0,
        max: 255,
        step: 1,
        default: 128,
        read_only: false,
    };
    assert_eq!(control.kind, DeviceKind::Integer);
    assert!(control.min <= control.default && control.default <= control.max);
}

#[cfg(feature = "serde")]
#[test]
fn a_device_control_serialises_its_name_as_text() {
    let control = DeviceControl {
        id: 1,
        name: DeviceName::new("Hue"),
        kind: DeviceKind::Menu,
        min: -128,
        max: 128,
        step: 1,
        default: 0,
        read_only: true,
    };
    let json = serde_json::to_string(&control).unwrap();
    assert!(json.contains("\"name\":\"Hue\""), "{json}");
    assert!(json.contains("\"kind\":\"menu\""), "{json}");
    let back: DeviceControl = serde_json::from_str(&json).unwrap();
    assert_eq!(back, control);
}

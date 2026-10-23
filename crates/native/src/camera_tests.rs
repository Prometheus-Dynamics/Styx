use styx_sensor::SensorDescription;

use super::*;
use crate::modes::sensor_modes;

#[test]
fn modes_are_selected_by_size_format_and_rate() {
    let desc = SensorDescription::from_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sensor/sensors/ov9782.toml"
    ))
    .unwrap();
    let modes = sensor_modes(&desc);
    let offered = [(0x3007, vec![FourCc::new(b"pBAA")])];
    let m = select_mode(&modes, &StreamSettings::new(1280, 800).fps(120), &offered).unwrap();
    assert_eq!((m.mode.as_str(), m.format.as_str()), ("1280x800", "raw10"));
    let s = StreamSettings::new(1280, 800).fourcc(FourCc::new(b"BA81"));
    assert_eq!(select_mode(&modes, &s, &offered).unwrap().format, "raw8");
    // The node offers only pBAA for raw10.
    let s = StreamSettings::new(1280, 800).fourcc(FourCc::new(b"BG10"));
    assert!(select_mode(&modes, &s, &offered).is_err());
    let s = StreamSettings::new(1280, 800).fps(121);
    assert!(
        select_mode(&modes, &s, &offered)
            .unwrap_err()
            .to_string()
            .contains("outside")
    );
    assert!(select_mode(&modes, &StreamSettings::new(1920, 1080), &offered).is_err());
    let exact = StreamSettings::new(1280, 800).interval(Fraction::new(82901, 10_000_000));
    assert!(select_mode(&modes, &exact, &offered).is_ok());
    assert_eq!(CameraOptions::default().buffers, 4);
}

//! The lens description format (TOML and the compiled postcard form) stays the same now that
//! its VCM types are Lemnos's (`lemnos_drivers_vcm::{VcmChip, OwnedVcmFormat}`): the bytes
//! and errors below were recorded with Styx's own mirror types before the switch (dev 28e24e7).

use styx_sensor::{KernelSensorData, LensDescription, SensorDescription};

/// Lens descriptions covering every chip, a custom format with and without defaults, and a
/// built-in chip with an overriding format.
const LENSES: &[&str] = &[
    "i2c = { address = 0x0c, chip = \"dw9714\" }",
    "i2c = { address = 0x0c, chip = \"dw9807\" }",
    "range = [0, 1023]\nsettle_us = 10000\nmap = [0.0, 445, 15.0, 925]\n\
     i2c = { address = 0x0c, chip = \"dw9817\" }",
    "i2c = { address = 0x0c, bus = 3, chip = \"ak7375\" }",
    "[i2c]\naddress = 0x0d\nchip = \"custom\"\n\
     format = { register = 0x10, power_up = [[0x02, 0x00]], power_up_us = 100 }",
    "[i2c]\naddress = 0x0c\nchip = \"custom\"\n\
     format = { bytes = 1, shift = 1, bits = 7, or = 1, power_down = [[0x80, 0x00], [0x01]] }",
    "[i2c]\naddress = 0x0c\nchip = \"dw9714\"\nformat = { or = 0x0f }",
    "i2c = { address = 0x0c, chip = \"custom\" }",
    "i2c = { address = 0x0c, chip = \"custom\", format = { bytes = 1 } }",
    "i2c = { address = 0x0c, chip = \"custom\", format = { bytes = 3 } }",
    "i2c = { address = 0x0c, chip = \"custom\", format = { power_up = [[1], [2], [3], [4], [5]] } }",
    "i2c = { address = 0x80, chip = \"dw9714\" }",
];

/// Rejected lens descriptions.
const BAD: &[&str] = &[
    "i2c = { address = 0x0c, chip = \"dw9999\" }",
    "i2c = { address = 0x0c, chip = \"DW9714\" }",
    "i2c = { address = 0x0c, chip = \"custom\", format = { speed = 1 } }",
    "i2c = { address = 0x0c, chip = \"custom\", format = { bytes = 300 } }",
];

/// Per entry of [`LENSES`]: the compiled form (hex), the TOML written back, the check.
const EXPECTED: &[(&str, &str, Option<&str>)] = &[
    (
        "00000000e05d0200010c000000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"dw9714\"\n",
        None,
    ),
    (
        "00000000e05d0200010c000100",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"dw9807\"\n",
        None,
    ),
    (
        "00000100fe0f00904e020400000000000000000000000000d07b400000000000002e400000000000e88c40010c000200",
        "drivers = []\nrange = [0, 1023]\nsettle_us = 10000\ndelay = 2\nmap = [0.0, 445.0, 15.0, 925.0]\n\n[i2c]\naddress = 12\nchip = \"dw9817\"\n",
        None,
    ),
    (
        "00000000e05d0200010c01030300",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nbus = 3\nchip = \"ak7375\"\n",
        None,
    ),
    (
        "00000000e05d0200010d000401011002000a00010202006400",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 13\nchip = \"custom\"\n\n[i2c.format]\nregister = 16\nbytes = 2\nshift = 0\nbits = 10\nor = 0\npower_up = [[2, 0]]\npower_up_us = 100\npower_down = []\n",
        None,
    ),
    (
        "00000000e05d0200010c00040100010107010000020280000101",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"custom\"\n\n[i2c.format]\nbytes = 1\nshift = 1\nbits = 7\nor = 1\npower_up = []\npower_up_us = 0\npower_down = [[128, 0], [1]]\n",
        None,
    ),
    (
        "00000000e05d0200010c0000010002000a0f000000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"dw9714\"\n\n[i2c.format]\nbytes = 2\nshift = 0\nbits = 10\nor = 15\npower_up = []\npower_up_us = 0\npower_down = []\n",
        None,
    ),
    (
        "00000000e05d0200010c000400",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"custom\"\n",
        Some("a custom VCM chip needs a `format`"),
    ),
    (
        "00000000e05d0200010c0004010001000a00000000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"custom\"\n\n[i2c.format]\nbytes = 1\nshift = 0\nbits = 10\nor = 0\npower_up = []\npower_up_us = 0\npower_down = []\n",
        Some("VCM format: position does not fit its bytes"),
    ),
    (
        "00000000e05d0200010c0004010003000a00000000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"custom\"\n\n[i2c.format]\nbytes = 3\nshift = 0\nbits = 10\nor = 0\npower_up = []\npower_up_us = 0\npower_down = []\n",
        Some("VCM format: bytes must be 1 or 2, bits at least 1"),
    ),
    (
        "00000000e05d0200010c0004010002000a0005010101020103010401050000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 12\nchip = \"custom\"\n\n[i2c.format]\nbytes = 2\nshift = 0\nbits = 10\nor = 0\npower_up = [[1], [2], [3], [4], [5]]\npower_up_us = 0\npower_down = []\n",
        Some("VCM format: at most 4 power-up and power-down writes"),
    ),
    (
        "00000000e05d0200018001000000",
        "drivers = []\nsettle_us = 12000\ndelay = 2\nmap = []\n\n[i2c]\naddress = 128\nchip = \"dw9714\"\n",
        Some("lens i2c address 0x80 is not 7-bit"),
    ),
];

/// Per entry of [`BAD`]: the end of the parse error.
const BAD_ERRORS: &[&str] = &[
    "unknown variant `dw9999`, expected one of `dw9714`, `dw9807`, `dw9817`, `ak7375`, `custom`",
    "unknown variant `DW9714`, expected one of `dw9714`, `dw9807`, `dw9817`, `ak7375`, `custom`",
    "unknown field `speed`, expected one of `register`, `bytes`, `shift`, `bits`, `or`, `power_up`, `power_up_us`, `power_down`",
    "invalid value: integer `300`, expected u8",
];

/// The IMX708 kernel data's lens, compiled.
const IMX708_LENS: &str = "01066477393831370206647739383037066477393831370100fe0f00e05d020400000000000000000000000000d07b400000000000002e400000000000e88c4000";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn lens_descriptions_parse_compile_and_check_as_before() {
    assert_eq!(LENSES.len(), EXPECTED.len());
    for (src, (compiled, written, check)) in LENSES.iter().zip(EXPECTED) {
        let d: LensDescription = toml::from_str(src).unwrap();
        let bytes = postcard::to_allocvec(&d).unwrap();
        assert_eq!(hex(&bytes), *compiled, "{src}");
        assert_eq!(postcard::from_bytes::<LensDescription>(&bytes).unwrap(), d);
        assert_eq!(toml::to_string(&d).unwrap(), *written, "{src}");
        assert_eq!(d.check().err().as_deref(), *check, "{src}");
    }
}

#[test]
fn bad_lens_descriptions_fail_as_before() {
    assert_eq!(BAD.len(), BAD_ERRORS.len());
    for (src, error) in BAD.iter().zip(BAD_ERRORS) {
        let e = toml::from_str::<LensDescription>(src)
            .unwrap_err()
            .to_string();
        assert!(e.trim_end().ends_with(error), "{src}: {e}");
    }
}

#[test]
fn builtin_lenses_compile_as_before() {
    let all = KernelSensorData::builtin();
    let lenses: Vec<String> = all
        .iter()
        .filter_map(|d| d.lens.as_ref())
        .map(|l| hex(&postcard::to_allocvec(l).unwrap()))
        .collect();
    assert_eq!(lenses, [IMX708_LENS]);
    for (name, src) in styx_sensor::BUILTIN_DESCRIPTIONS {
        let d = SensorDescription::from_toml_str(src, name).unwrap();
        let bytes = postcard::to_allocvec(&d).unwrap();
        assert_eq!(
            postcard::from_bytes::<SensorDescription>(&bytes).unwrap(),
            d
        );
    }
}

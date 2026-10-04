//! Embedded data lines (what the sensor sends ahead of each frame) through every layout that
//! ships: the OV9782's register list and the CCS/SMIA tagged lines of the kernel-driver
//! sensors (IMX219, IMX477, IMX708, ...), packed and unpacked.

#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use styx_sensor::{
    BUILTIN_DESCRIPTIONS, EmbeddedPacking, KernelSensorData, SensorDescription,
    embedded_unpack_raw10,
};

fn layouts() -> &'static [SensorDescription] {
    static DESCS: OnceLock<Vec<SensorDescription>> = OnceLock::new();
    DESCS.get_or_init(|| {
        let base = SensorDescription::from_toml_str(BUILTIN_DESCRIPTIONS[0].1, "builtin")
            .expect("built-in description");
        let mut all = vec![base.clone()];
        for data in KernelSensorData::builtin() {
            let Some(layout) = data.embedded_data else {
                continue;
            };
            for packing in [
                EmbeddedPacking::None,
                EmbeddedPacking::Raw10,
                EmbeddedPacking::Raw12,
            ] {
                let mut d = base.clone();
                d.embedded_data = Some(styx_sensor::EmbeddedData {
                    packing,
                    ..layout.clone()
                });
                all.push(d);
            }
        }
        all
    })
}

fuzz_target!(|line: &[u8]| {
    let unpacked = embedded_unpack_raw10(line);
    assert_eq!(unpacked.len(), line.len() / 5 * 4);
    for d in layouts() {
        let _ = d.embedded_registers(line);
        let _ = d.decode_embedded(line);
        let _ = d.embedded_unpacked(line);
    }
});

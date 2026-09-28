//! Reading applied control values back from a frame's embedded data.

use std::collections::BTreeMap;

use crate::desc::{Field, SensorDescription};
use crate::schedule::{Control, ControlSet};

impl SensorDescription {
    /// Register bytes found in unpacked embedded data, by address. Empty without a layout.
    pub fn embedded_registers(&self, data: &[u8]) -> BTreeMap<u16, u8> {
        let Some(layout) = &self.embedded_data else {
            return BTreeMap::new();
        };
        layout
            .entries
            .iter()
            .filter_map(|e| {
                data.get(usize::try_from(e.offset).ok()?)
                    .map(|b| (e.address, *b))
            })
            .collect()
    }

    /// The control codes that can be read back from unpacked embedded data (the same units as
    /// [`ControlScheduler`](crate::ControlScheduler) uses), for
    /// [`ControlScheduler::report`](crate::ControlScheduler::report).
    pub fn decode_embedded(&self, data: &[u8]) -> ControlSet {
        let regs = self.embedded_registers(data);
        let read = |f: &Field| -> Option<u32> {
            (0..f.bytes).try_fold(0u32, |acc, i| {
                Some((acc << 8) | u32::from(*regs.get(&f.address.checked_add(u16::from(i))?)?))
            })
        };
        let ctl = &self.controls;
        let mut set = ControlSet::new();
        if let Some(v) = ctl
            .frame_length
            .as_ref()
            .and_then(|f| read(f).map(|r| f.decode(r)))
        {
            set.set(Control::FrameLength, v);
        }
        if let Some(f) = ctl.exposure.register {
            let fb = ctl.exposure.fraction_bits;
            let wide = Field {
                shift: f.shift - fb,
                bits: Some(f.bits() + fb),
                ..f
            };
            if let Some(r) = read(&f) {
                set.set(Control::Exposure, wide.decode(r));
            }
        }
        if let Some(v) = ctl
            .analog_gain
            .register
            .and_then(|f| read(&f).map(|r| f.decode(r)))
        {
            set.set(Control::AnalogGain, v);
        }
        if let Some(v) = ctl
            .digital_gain
            .as_ref()
            .and_then(|g| g.register)
            .and_then(|f| read(&f).map(|r| f.decode(r)))
        {
            set.set(Control::DigitalGain, v);
        }
        set
    }
}

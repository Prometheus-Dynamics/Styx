//! Reading applied control values back from a frame's embedded data.

use std::collections::BTreeMap;

use crate::desc::{EmbeddedControlKind, EmbeddedPacking, Field, SensorDescription};
use crate::schedule::{Control, ControlSet};

/// Unpacks CSI-2 RAW10 data: each group of five bytes gives four 10-bit words (high 8 bits,
/// then the low 2 bits of all four in the fifth byte); each word is returned as one byte
/// (its low 8 bits: embedded lines carry one byte value per word). A partial group at the end
/// is dropped.
pub fn unpack_raw10_bytes(data: &[u8]) -> Vec<u8> {
    data.chunks_exact(5)
        .flat_map(|g| {
            let low = g[4];
            (0..4).map(move |i| ((u16::from(g[i]) << 2) | u16::from((low >> (2 * i)) & 3)) as u8)
        })
        .collect()
}

impl SensorDescription {
    /// The embedded data as the layout's offsets count it: unpacked if the layout says the
    /// bytes are packed, else as given.
    pub fn embedded_unpacked(&self, data: &[u8]) -> Vec<u8> {
        match self.embedded_data.as_ref().map(|e| e.packing) {
            Some(EmbeddedPacking::Raw10) => unpack_raw10_bytes(data),
            _ => data.to_vec(),
        }
    }

    /// Register bytes found in embedded data (as received), by address. Empty without a
    /// layout.
    pub fn embedded_registers(&self, data: &[u8]) -> BTreeMap<u16, u8> {
        let Some(layout) = &self.embedded_data else {
            return BTreeMap::new();
        };
        let unpacked = self.embedded_unpacked(data);
        let data = unpacked.as_slice();
        layout
            .entries
            .iter()
            .filter_map(|e| {
                data.get(usize::try_from(e.offset).ok()?)
                    .map(|b| (e.address, *b))
            })
            .collect()
    }

    /// The control codes that can be read back from embedded data as received (the same units
    /// as [`ControlScheduler`](crate::ControlScheduler) uses), for
    /// [`ControlScheduler::report`](crate::ControlScheduler::report). `controls` entries of
    /// the layout take precedence over register bytes.
    pub fn decode_embedded(&self, data: &[u8]) -> ControlSet {
        let mut set = self.decode_embedded_registers(data);
        let Some(layout) = &self.embedded_data else {
            return set;
        };
        if layout.controls.is_empty() {
            return set;
        }
        let unpacked = self.embedded_unpacked(data);
        for c in &layout.controls {
            let start = c.offset as usize;
            let Some(bytes) = unpacked.get(start..start + usize::from(c.bytes)) else {
                continue;
            };
            let v = bytes.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b)) << c.shift;
            let control = match c.control {
                EmbeddedControlKind::Exposure => Control::Exposure,
                EmbeddedControlKind::AnalogGain => Control::AnalogGain,
                EmbeddedControlKind::DigitalGain => Control::DigitalGain,
                EmbeddedControlKind::FrameLength => Control::FrameLength,
            };
            set.set(control, v);
        }
        set
    }

    fn decode_embedded_registers(&self, data: &[u8]) -> ControlSet {
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

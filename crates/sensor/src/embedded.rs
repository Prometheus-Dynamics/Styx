//! Reading applied control values back from a frame's embedded data.

use alloc::collections::BTreeMap;
#[cfg(test)]
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::desc::{
    EmbeddedControlKind, EmbeddedData, EmbeddedFormat, EmbeddedPacking, Field, SensorDescription,
};
use crate::fixed::FixedVec;
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

/// Byte `k` of the embedded data as the layout's offsets count it (unpacked when the layout
/// says RAW10), without unpacking the rest: `None` past the data (or its last whole group).
fn unpacked_byte(packing: EmbeddedPacking, data: &[u8], k: usize) -> Option<u8> {
    match packing {
        EmbeddedPacking::Raw10 => {
            let g = data.get(5 * (k / 4)..5 * (k / 4) + 5)?;
            let i = k % 4;
            Some(((u16::from(g[i]) << 2) | u16::from((g[4] >> (2 * i)) & 3)) as u8)
        }
        _ => data.get(k).copied(),
    }
}

/// Register addresses an embedded layout reads, sorted and without repeats (at most
/// [`EMBEDDED_REGISTERS`]; validation keeps layouts within it).
type Addresses = FixedVec<u16, EMBEDDED_REGISTERS>;

/// Most register bytes a layout reads back from embedded data.
pub const EMBEDDED_REGISTERS: usize = 64;

impl SensorDescription {
    /// The embedded data as the layout's offsets count it: unpacked if the layout says the
    /// bytes are packed, else as given.
    pub fn embedded_unpacked(&self, data: &[u8]) -> Vec<u8> {
        match self.embedded_data.as_ref().map(|e| e.packing) {
            Some(EmbeddedPacking::Raw10) => unpack_raw10_bytes(data),
            _ => data.to_vec(),
        }
    }

    /// The start of `data` (as received) that holds every byte the layout reads: decoding
    /// touches only that (embedded buffers are often uncached, and a line is kilobytes long).
    fn embedded_prefix<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let Some(layout) = &self.embedded_data else {
            return &data[..0];
        };
        let entries = layout.entries.iter().map(|e| e.offset as usize + 1);
        let controls = layout
            .controls
            .iter()
            .map(|c| c.offset as usize + usize::from(c.bytes));
        let needed = entries.chain(controls).max().unwrap_or(0);
        let packed = match layout.packing {
            EmbeddedPacking::Raw10 => needed.div_ceil(4) * 5,
            _ => needed,
        };
        &data[..packed.min(data.len())]
    }

    /// Register addresses the layout reads (the bytes of `entries`, `registers` and the
    /// control register fields), sorted, without repeats.
    fn embedded_addresses(&self, layout: &EmbeddedData) -> Addresses {
        let ctl = &self.controls;
        let fields = [
            ctl.frame_length,
            ctl.exposure.register,
            ctl.analog_gain.register,
            ctl.digital_gain.as_ref().and_then(|g| g.register),
        ];
        let spans = fields
            .into_iter()
            .flatten()
            .map(|f| (f.address, f.bytes))
            .chain(layout.registers.iter().map(|r| (r.address, r.bytes)));
        let mut out = Addresses::new();
        let all = layout
            .entries
            .iter()
            .map(|e| e.address)
            .chain(spans.flat_map(|(a, n)| (0..u16::from(n)).map(move |i| a.wrapping_add(i))));
        for a in all {
            if let Err(at) = out.binary_search(&a) {
                if out.push(a).is_err() {
                    break;
                }
                out[at..].rotate_right(1);
            }
        }
        out
    }

    /// Register bytes found in embedded data (as received), by address. Empty without a
    /// layout.
    pub fn embedded_registers(&self, data: &[u8]) -> BTreeMap<u16, u8> {
        let Some(layout) = &self.embedded_data else {
            return BTreeMap::new();
        };
        if layout.format == EmbeddedFormat::Ccs {
            let wanted = self.embedded_addresses(layout);
            let mut out = BTreeMap::new();
            ccs_scan(data, layout.packing, &wanted, |a, v| {
                out.insert(a, v);
            });
            return out;
        }
        let unpacked = self.embedded_unpacked(self.embedded_prefix(data));
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
        if layout.controls.is_empty() || layout.format == EmbeddedFormat::Ccs {
            return set;
        }
        for c in &layout.controls {
            let start = c.offset as usize;
            let v = (start..start + usize::from(c.bytes)).try_fold(0u32, |a, k| {
                Some((a << 8) | u32::from(unpacked_byte(layout.packing, data, k)?))
            });
            if let Some(v) = v {
                set.set(kind_control(c.control), v << c.shift);
            }
        }
        set
    }

    /// Without allocating (it runs for every frame): register bytes are looked up in the
    /// line, or collected from a CCS line into a small fixed map.
    fn decode_embedded_registers(&self, data: &[u8]) -> ControlSet {
        let Some(layout) = &self.embedded_data else {
            return ControlSet::new();
        };
        let mut ccs = FixedVec::<(u16, u8), EMBEDDED_REGISTERS>::new();
        if layout.format == EmbeddedFormat::Ccs {
            let wanted = self.embedded_addresses(layout);
            ccs_scan(data, layout.packing, &wanted, |a, v| {
                let _ = ccs.push((a, v));
            });
        }
        let byte = |address: u16| -> Option<u8> {
            if layout.format == EmbeddedFormat::Ccs {
                return ccs.iter().find(|(a, _)| *a == address).map(|(_, v)| *v);
            }
            let e = layout.entries.iter().find(|e| e.address == address)?;
            unpacked_byte(layout.packing, data, usize::try_from(e.offset).ok()?)
        };
        let read = |f: &Field| -> Option<u32> {
            (0..f.bytes).try_fold(0u32, |acc, i| {
                Some((acc << 8) | u32::from(byte(f.address.checked_add(u16::from(i))?)?))
            })
        };
        let ctl = &self.controls;
        let mut set = ControlSet::new();
        for r in self.embedded_data.iter().flat_map(|l| &l.registers) {
            if let Some(v) = read(&Field::whole(r.address, r.bytes)) {
                set.set(kind_control(r.control), v << r.shift);
            }
        }
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

fn kind_control(kind: EmbeddedControlKind) -> Control {
    match kind {
        EmbeddedControlKind::Exposure => Control::Exposure,
        EmbeddedControlKind::AnalogGain => Control::AnalogGain,
        EmbeddedControlKind::DigitalGain => Control::DigitalGain,
        EmbeddedControlKind::FrameLength => Control::FrameLength,
    }
}

/// CCS tags (MIPI CCS, "embedded data line" format; SMIA before it).
const CCS_LINE_START: u8 = 0x0a;
const CCS_ADDRESS_HIGH: u8 = 0xaa;
const CCS_ADDRESS_LOW: u8 = 0xa5;
const CCS_VALUE: u8 = 0x5a;
const CCS_SKIP: u8 = 0x55;
const CCS_LINE_END: u8 = 0x07;

/// The `wanted` registers of a CCS tagged embedded line (see [`EmbeddedFormat::Ccs`]). Stops
/// once all are found, at the line end, or at an unknown tag; a line not starting with the
/// start code gives nothing.
#[cfg(test)]
fn ccs_registers(
    data: &[u8],
    packing: EmbeddedPacking,
    wanted: &BTreeSet<u16>,
) -> BTreeMap<u16, u8> {
    let wanted: alloc::vec::Vec<u16> = wanted.iter().copied().collect();
    let mut out = BTreeMap::new();
    ccs_scan(data, packing, &wanted, |a, v| {
        out.insert(a, v);
    });
    out
}

/// [`ccs_registers`] over `wanted` sorted without repeats, giving each register found to
/// `found` (once per register): no allocation.
fn ccs_scan(data: &[u8], packing: EmbeddedPacking, wanted: &[u16], mut found: impl FnMut(u16, u8)) {
    let pad = match packing {
        EmbeddedPacking::None => usize::MAX,
        EmbeddedPacking::Raw10 => 5,
        EmbeddedPacking::Raw12 => 3,
    };
    let mut bytes = data
        .iter()
        .enumerate()
        .filter(|(i, _)| pad == usize::MAX || i % pad != pad - 1)
        .map(|(_, b)| *b);
    if bytes.next() != Some(CCS_LINE_START) || wanted.is_empty() {
        return;
    }
    let mut seen = 0usize;
    // Bit i: wanted[i] already found (a line repeats no register, but be safe).
    let mut done = [0u64; EMBEDDED_REGISTERS.div_ceil(64)];
    let mut address: u16 = 0;
    while let (Some(tag), Some(value)) = (bytes.next(), bytes.next()) {
        match tag {
            CCS_ADDRESS_HIGH => address = (address & 0x00ff) | (u16::from(value) << 8),
            CCS_ADDRESS_LOW => address = (address & 0xff00) | u16::from(value),
            CCS_VALUE => {
                if let Ok(i) = wanted.binary_search(&address)
                    && i < EMBEDDED_REGISTERS
                    && done[i / 64] & (1 << (i % 64)) == 0
                {
                    done[i / 64] |= 1 << (i % 64);
                    found(address, value);
                    seen += 1;
                    if seen == wanted.len().min(EMBEDDED_REGISTERS) {
                        break;
                    }
                }
                address = address.wrapping_add(1);
            }
            CCS_SKIP => address = address.wrapping_add(1),
            CCS_LINE_END => break,
            // An unknown tag: not a CCS line after all.
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CCS line: registers `0x0200..` from `values`, with `0x0202` skipped, padded as RAW10
    /// when asked.
    fn ccs_line(values: &[u8], raw10: bool) -> Vec<u8> {
        let mut pairs = vec![
            CCS_LINE_START,
            CCS_ADDRESS_HIGH,
            0x02,
            CCS_ADDRESS_LOW,
            0x00,
        ];
        for (i, v) in values.iter().enumerate() {
            pairs.extend([if i == 2 { CCS_SKIP } else { CCS_VALUE }, *v]);
        }
        pairs.extend([CCS_LINE_END, CCS_LINE_END]);
        if !raw10 {
            return pairs;
        }
        pairs
            .chunks(4)
            .flat_map(|c| c.iter().copied().chain(core::iter::once(CCS_SKIP)))
            .collect()
    }

    #[test]
    fn ccs_lines_give_registers_by_address() {
        let wanted: BTreeSet<u16> = [0x0201, 0x0202, 0x0203, 0x0204].into();
        for raw10 in [false, true] {
            let line = ccs_line(&[1, 2, 3, 4, 5], raw10);
            let packing = if raw10 {
                EmbeddedPacking::Raw10
            } else {
                EmbeddedPacking::None
            };
            let r = ccs_registers(&line, packing, &wanted);
            // 0x0202 was skipped.
            assert_eq!(
                r,
                BTreeMap::from([(0x0201, 2), (0x0203, 4), (0x0204, 5)]),
                "raw10 {raw10}"
            );
        }
        assert!(ccs_registers(&[0, 1, 2], EmbeddedPacking::None, &wanted).is_empty());
        let mut line = ccs_line(&[1, 2, 3, 4, 5], false);
        line[5] = 0x99; // an unknown tag ends the parse
        assert!(ccs_registers(&line, EmbeddedPacking::None, &wanted).is_empty());
    }
}

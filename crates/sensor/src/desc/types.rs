//! Small value types used throughout the schema.

use serde::{Deserialize, Serialize};

/// A width and height in pixels. In TOML: `[1280, 800]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(from = "[u32; 2]", into = "[u32; 2]")]
pub struct Size {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Size {
    /// A size.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

impl From<Size> for [u32; 2] {
    fn from(s: Size) -> Self {
        [s.width, s.height]
    }
}

impl From<[u32; 2]> for Size {
    fn from([width, height]: [u32; 2]) -> Self {
        Self { width, height }
    }
}

/// A rectangle in pixel-array coordinates. In TOML:
/// `{ left = 8, top = 8, width = 1280, height = 800 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    /// Left edge.
    pub left: u32,
    /// Top edge.
    pub top: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Rect {
    /// True when `other` lies entirely inside `self`.
    pub fn contains(&self, other: &Rect) -> bool {
        other.left >= self.left
            && other.top >= self.top
            && u64::from(other.left) + u64::from(other.width)
                <= u64::from(self.left) + u64::from(self.width)
            && u64::from(other.top) + u64::from(other.height)
                <= u64::from(self.top) + u64::from(self.height)
    }
}

/// A value field inside a register. Registers wider than one byte are consecutive addresses,
/// most significant byte first (the usual convention for CSI-2 sensors).
///
/// In TOML: `{ address = 0x3500, bytes = 3, shift = 4, bits = 16 }`. `bytes` defaults to 1,
/// `shift` to 0 and `bits` to the rest of the register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// First (most significant) register address.
    pub address: u16,
    /// Register width in bytes, 1 to 4.
    #[serde(default = "one")]
    pub bytes: u8,
    /// Bit position of the field's least significant bit.
    #[serde(default)]
    pub shift: u8,
    /// Field width in bits (default: `bytes * 8 - shift`).
    #[serde(default)]
    pub bits: Option<u8>,
    /// Read the register and keep the bits outside the field (default: write them as 0).
    #[serde(default)]
    pub read_modify_write: bool,
}

fn one() -> u8 {
    1
}

impl Field {
    /// A whole register of `bytes` bytes.
    pub const fn whole(address: u16, bytes: u8) -> Self {
        Self {
            address,
            bytes,
            shift: 0,
            bits: None,
            read_modify_write: false,
        }
    }

    /// Field width in bits.
    pub fn bits(&self) -> u8 {
        self.bits
            .unwrap_or_else(|| (self.bytes * 8).saturating_sub(self.shift))
    }

    /// Largest value the field holds.
    pub fn max_value(&self) -> u32 {
        mask(self.bits())
    }

    /// Mask of the field's bits within the register.
    pub fn register_mask(&self) -> u32 {
        mask(self.bits()) << self.shift
    }

    /// Register value for `value`, merging into `current` (the other bits) when given.
    pub fn encode(&self, value: u32, current: Option<u32>) -> u32 {
        let placed = (value & mask(self.bits())) << self.shift;
        match current {
            Some(c) => (c & !self.register_mask()) | placed,
            None => placed,
        }
    }

    /// The field's value in a register value.
    pub fn decode(&self, register: u32) -> u32 {
        (register >> self.shift) & mask(self.bits())
    }
}

fn mask(bits: u8) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    }
}

/// A blanking range with a default, in pixels (horizontal) or lines (vertical).
/// In TOML: `{ min = 110, max = 51540, default = 1022 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Blanking {
    /// Minimum.
    pub min: u32,
    /// Maximum.
    pub max: u32,
    /// Value used when the mode is applied.
    pub default: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_encode_decode() {
        let f = Field {
            address: 0x3500,
            bytes: 3,
            shift: 4,
            bits: Some(16),
            read_modify_write: false,
        };
        assert_eq!(f.encode(642, None), 642 << 4);
        assert_eq!(f.decode(642 << 4 | 0xf), 642);
        assert_eq!(f.max_value(), 0xffff);
        let flip = Field {
            address: 0x3820,
            bytes: 1,
            shift: 2,
            bits: Some(1),
            read_modify_write: true,
        };
        assert_eq!(flip.encode(1, Some(0x40)), 0x44);
        assert_eq!(flip.encode(0, Some(0x3c)), 0x38);
        assert_eq!(Field::whole(0x380e, 2).bits(), 16);
    }

    #[test]
    fn rect_contains() {
        let a = Rect {
            left: 0,
            top: 0,
            width: 1296,
            height: 816,
        };
        assert!(a.contains(&Rect {
            left: 8,
            top: 8,
            width: 1280,
            height: 800
        }));
        assert!(!a.contains(&Rect {
            left: 17,
            top: 8,
            width: 1280,
            height: 800
        }));
    }
}

//! Device controls: what a backend passes through when a device control has no Styx control.
//!
//! A backend lists every control its device offers as a [`DeviceControl`]. Each one is either
//! mapped to a named [`Control`](crate::Control) or left as a passthrough, and the passthrough
//! values are read and written by id. They are not part of [`FrameMetadata`](crate::FrameMetadata).

use core::fmt;
use core::str;

/// Bytes a [`DeviceName`] holds. A V4L2 control name is at most 32 bytes, including its nul.
pub const DEVICE_NAME_CAPACITY: usize = 32;

/// A device control's name, held inline: no heap, `Copy`. Longer names are cut at a character
/// boundary to [`DEVICE_NAME_CAPACITY`] bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceName {
    len: u8,
    bytes: [u8; DEVICE_NAME_CAPACITY],
}

impl DeviceName {
    /// The name `text`, cut to fit.
    pub fn new(text: &str) -> Self {
        let mut end = text.len().min(DEVICE_NAME_CAPACITY);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; DEVICE_NAME_CAPACITY];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Self {
            len: end as u8,
            bytes,
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl Default for DeviceName {
    fn default() -> Self {
        Self::new("")
    }
}

impl fmt::Display for DeviceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for DeviceName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for DeviceName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameVisitor;
        impl serde::de::Visitor<'_> for NameVisitor {
            type Value = DeviceName;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a control name")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<DeviceName, E> {
                Ok(DeviceName::new(v))
            }
        }
        deserializer.deserialize_str(NameVisitor)
    }
}

/// The kind of a device control's value. Every value is an `i64` on the passthrough: a boolean
/// is 0 or 1, a menu or bitmask is its index or bits, a button takes no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DeviceKind {
    /// A 32-bit integer.
    Integer,
    /// A 64-bit integer.
    Integer64,
    /// On or off (0 or 1).
    Boolean,
    /// A menu: the value is the index of one of its items.
    Menu,
    /// A bitmask: the value is the set bits.
    Bitmask,
    /// An action with no value: setting it performs the action.
    Button,
    /// Anything else (strings, arrays, compound values, class headings). Listed, not settable.
    Other,
}

/// One control a device offers, as described: its id, name, kind and bounds. The value is read
/// and written by `id` on the backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DeviceControl {
    /// The backend's id for the control (a V4L2 `V4L2_CID_*`, for example).
    pub id: u32,
    /// The control's name, as the device names it.
    pub name: DeviceName,
    /// The kind of value.
    pub kind: DeviceKind,
    /// Smallest value (menus: the first item index).
    pub min: i64,
    /// Largest value (menus: the last item index).
    pub max: i64,
    /// Step between valid values (0 when the device does not say).
    pub step: u64,
    /// The device's default value.
    pub default: i64,
    /// Whether the device refuses writes to this control.
    pub read_only: bool,
}

//! Camera terminal and processing unit controls, named and numbered like `uvcvideo`'s V4L2
//! controls (same ids, units and menus), so code written for a UVC camera through V4L2 works
//! unchanged on the userspace backend.

/// Class-specific requests (UVC 1.5 §A.8).
pub mod request {
    pub const SET_CUR: u8 = 0x01;
    pub const GET_CUR: u8 = 0x81;
    pub const GET_MIN: u8 = 0x82;
    pub const GET_MAX: u8 = 0x83;
    pub const GET_RES: u8 = 0x84;
    pub const GET_LEN: u8 = 0x85;
    pub const GET_INFO: u8 = 0x86;
    pub const GET_DEF: u8 = 0x87;
}

/// Which entity a control belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    CameraTerminal,
    ProcessingUnit,
}

/// How a control's bytes map to a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Signed,
    Unsigned,
    Bool,
    /// A menu whose V4L2 values equal the UVC values (power line frequency).
    Menu,
    /// `CT_AE_MODE`: a bitmap (1 manual, 2 auto, 4 shutter priority, 8 aperture priority),
    /// presented as V4L2's `V4L2_CID_EXPOSURE_AUTO` menu (0 auto, 1 manual, 2 shutter
    /// priority, 3 aperture priority).
    AeMode,
}

/// A control Styx knows how to drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlDef {
    /// The V4L2 control id `uvcvideo` gives it (the Styx `ControlId`).
    pub id: u32,
    pub name: &'static str,
    pub unit: Unit,
    /// The control selector.
    pub selector: u8,
    /// Its bit in the entity's `bmControls`.
    pub bit: u8,
    /// Bytes of its value.
    pub size: u8,
    pub kind: Kind,
}

const fn pu(
    id: u32,
    name: &'static str,
    selector: u8,
    bit: u8,
    size: u8,
    kind: Kind,
) -> ControlDef {
    ControlDef {
        id,
        name,
        unit: Unit::ProcessingUnit,
        selector,
        bit,
        size,
        kind,
    }
}

const fn ct(
    id: u32,
    name: &'static str,
    selector: u8,
    bit: u8,
    size: u8,
    kind: Kind,
) -> ControlDef {
    ControlDef {
        id,
        name,
        unit: Unit::CameraTerminal,
        selector,
        bit,
        size,
        kind,
    }
}

/// V4L2 control ids (`<linux/v4l2-controls.h>`).
pub mod ids {
    pub const BRIGHTNESS: u32 = 0x0098_0900;
    pub const CONTRAST: u32 = 0x0098_0901;
    pub const SATURATION: u32 = 0x0098_0902;
    pub const HUE: u32 = 0x0098_0903;
    pub const AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
    pub const GAMMA: u32 = 0x0098_0910;
    pub const GAIN: u32 = 0x0098_0913;
    pub const POWER_LINE_FREQUENCY: u32 = 0x0098_0918;
    pub const HUE_AUTO: u32 = 0x0098_0919;
    pub const WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;
    pub const EXPOSURE_AUTO: u32 = 0x009a_0901;
    /// Exposure time in 100 µs units.
    pub const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    pub const EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
    pub const FOCUS_ABSOLUTE: u32 = 0x009a_090a;
    pub const FOCUS_AUTO: u32 = 0x009a_090c;
    pub const ZOOM_ABSOLUTE: u32 = 0x009a_090d;
    pub const PRIVACY: u32 = 0x009a_0910;
    pub const IRIS_ABSOLUTE: u32 = 0x009a_0911;
}

/// Every control, processing unit first.
pub const CONTROLS: &[ControlDef] = &[
    pu(ids::BRIGHTNESS, "brightness", 0x02, 0, 2, Kind::Signed),
    pu(ids::CONTRAST, "contrast", 0x03, 1, 2, Kind::Unsigned),
    pu(ids::HUE, "hue", 0x06, 2, 2, Kind::Signed),
    pu(ids::SATURATION, "saturation", 0x07, 3, 2, Kind::Unsigned),
    pu(ids::SHARPNESS, "sharpness", 0x08, 4, 2, Kind::Unsigned),
    pu(ids::GAMMA, "gamma", 0x09, 5, 2, Kind::Unsigned),
    pu(
        ids::WHITE_BALANCE_TEMPERATURE,
        "white_balance_temperature",
        0x0a,
        6,
        2,
        Kind::Unsigned,
    ),
    pu(
        ids::BACKLIGHT_COMPENSATION,
        "backlight_compensation",
        0x01,
        8,
        2,
        Kind::Unsigned,
    ),
    pu(ids::GAIN, "gain", 0x04, 9, 2, Kind::Unsigned),
    pu(
        ids::POWER_LINE_FREQUENCY,
        "power_line_frequency",
        0x05,
        10,
        1,
        Kind::Menu,
    ),
    pu(ids::HUE_AUTO, "hue_auto", 0x10, 11, 1, Kind::Bool),
    pu(
        ids::AUTO_WHITE_BALANCE,
        "white_balance_automatic",
        0x0b,
        12,
        1,
        Kind::Bool,
    ),
    ct(
        ids::EXPOSURE_AUTO,
        "auto_exposure",
        0x02,
        1,
        1,
        Kind::AeMode,
    ),
    ct(
        ids::EXPOSURE_AUTO_PRIORITY,
        "exposure_dynamic_framerate",
        0x03,
        2,
        1,
        Kind::Bool,
    ),
    ct(
        ids::EXPOSURE_ABSOLUTE,
        "exposure_time_absolute",
        0x04,
        3,
        4,
        Kind::Unsigned,
    ),
    ct(
        ids::FOCUS_ABSOLUTE,
        "focus_absolute",
        0x06,
        5,
        2,
        Kind::Unsigned,
    ),
    ct(
        ids::IRIS_ABSOLUTE,
        "iris_absolute",
        0x09,
        7,
        2,
        Kind::Unsigned,
    ),
    ct(
        ids::ZOOM_ABSOLUTE,
        "zoom_absolute",
        0x0b,
        9,
        2,
        Kind::Unsigned,
    ),
    ct(
        ids::FOCUS_AUTO,
        "focus_automatic_continuous",
        0x08,
        17,
        1,
        Kind::Bool,
    ),
    ct(ids::PRIVACY, "privacy", 0x11, 18, 1, Kind::Bool),
];

/// A control by V4L2 id.
pub fn find(id: u32) -> Option<&'static ControlDef> {
    CONTROLS.iter().find(|c| c.id == id)
}

/// `CT_AE_MODE` bits for V4L2's exposure menu values 0..=3.
const AE_MODES: [u8; 4] = [2, 1, 4, 8];

impl ControlDef {
    /// The value of the control's bytes (little-endian, sign-extended for signed controls),
    /// in V4L2 terms.
    pub fn decode(&self, b: &[u8]) -> i64 {
        let mut raw = [0u8; 8];
        let n = b.len().min(usize::from(self.size)).min(8);
        raw[..n].copy_from_slice(&b[..n]);
        let u = u64::from_le_bytes(raw);
        match self.kind {
            Kind::Signed => {
                let shift = 64 - 8 * u32::from(self.size);
                ((u << shift) as i64) >> shift
            }
            Kind::Unsigned | Kind::Menu => u as i64,
            Kind::Bool => i64::from(u != 0),
            Kind::AeMode => AE_MODES
                .iter()
                .position(|&m| u as u8 & m != 0)
                .map_or(0, |i| i as i64),
        }
    }

    /// The control's bytes for a V4L2 value.
    pub fn encode(&self, v: i64) -> Vec<u8> {
        let raw: u64 = match self.kind {
            Kind::AeMode => u64::from(*AE_MODES.get(v.clamp(0, 3) as usize).unwrap_or(&2)),
            Kind::Bool => u64::from(v != 0),
            _ => v as u64,
        };
        raw.to_le_bytes()[..usize::from(self.size)].to_vec()
    }

    /// For `CT_AE_MODE`: the V4L2 menu values a `GET_RES` bitmap allows.
    pub fn ae_menu(res_bitmap: u8) -> Vec<i64> {
        (0..4)
            .filter(|&i| res_bitmap & AE_MODES[i] != 0)
            .map(|i| i as i64)
            .collect()
    }
}

/// `GET_INFO` capability bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info(pub u8);

impl Info {
    pub fn get(self) -> bool {
        self.0 & 0x01 != 0
    }
    pub fn set(self) -> bool {
        self.0 & 0x02 != 0
    }
    /// Disabled because of an automatic mode.
    pub fn disabled(self) -> bool {
        self.0 & 0x04 != 0
    }
    pub fn autoupdate(self) -> bool {
        self.0 & 0x08 != 0
    }
    pub fn asynchronous(self) -> bool {
        self.0 & 0x10 != 0
    }
}

/// A control the camera has, with its range as it reported it (V4L2 terms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlInfo {
    pub def: &'static ControlDef,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub default: i64,
    pub info: Info,
}

/// The controls an entity's `bmControls` announces.
pub fn announced(unit: Unit, bitmap: u64) -> impl Iterator<Item = &'static ControlDef> {
    CONTROLS
        .iter()
        .filter(move |c| c.unit == unit && bitmap & (1u64 << c.bit) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        let b = find(ids::BRIGHTNESS).unwrap();
        assert_eq!(b.encode(-64), vec![0xc0, 0xff]);
        assert_eq!(b.decode(&[0xc0, 0xff]), -64);
        let e = find(ids::EXPOSURE_ABSOLUTE).unwrap();
        assert_eq!(e.decode(&e.encode(3125)), 3125);
        assert_eq!(e.encode(3125).len(), 4);
        let g = find(ids::GAIN).unwrap();
        assert_eq!(g.decode(&[0xff, 0x00]), 255);
    }

    #[test]
    fn exposure_modes_map_like_uvcvideo() {
        let ae = find(ids::EXPOSURE_AUTO).unwrap();
        // V4L2_EXPOSURE_MANUAL (1) is UVC manual (1); aperture priority (3) is 8.
        assert_eq!(ae.encode(1), vec![1]);
        assert_eq!(ae.encode(3), vec![8]);
        assert_eq!(ae.decode(&[8]), 3);
        assert_eq!(ae.decode(&[2]), 0);
        assert_eq!(ControlDef::ae_menu(0x09), vec![1, 3]);
    }

    #[test]
    fn bitmaps_announce_controls() {
        // C270 processing unit: brightness..gamma? 0x033f = bits 0-5, 8, 9.
        let pu: Vec<_> = announced(Unit::ProcessingUnit, 0x0000_073f)
            .map(|c| c.name)
            .collect();
        assert!(pu.contains(&"brightness") && pu.contains(&"gain"));
        assert!(pu.contains(&"power_line_frequency"));
        assert!(!pu.contains(&"hue_auto"));
        let ct: Vec<_> = announced(Unit::CameraTerminal, 0x0e)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            ct,
            vec![
                "auto_exposure",
                "exposure_dynamic_framerate",
                "exposure_time_absolute"
            ]
        );
    }
}

//! Controls: query (including extended, compound and menu controls), get and set through the
//! extended control API, optionally within a media request. Shared by video nodes and
//! subdevices through the [`Controls`] trait.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use super::raw::{self, zeroed};
use crate::flags::flags;
use crate::ioctl::{self, cstr_field};
use crate::{Error, Result};

const NEXT_CTRL: u32 = 0x8000_0000;
const NEXT_COMPOUND: u32 = 0x4000_0000;

const WHICH_CUR_VAL: u32 = 0;
const WHICH_DEF_VAL: u32 = 0x0f00_0000;
const WHICH_REQUEST_VAL: u32 = 0x0f01_0000;
const WHICH_MIN_VAL: u32 = 0x0f02_0000;
const WHICH_MAX_VAL: u32 = 0x0f03_0000;

/// The type of a control (`enum v4l2_ctrl_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControlType {
    /// 32-bit integer.
    Integer,
    /// Boolean.
    Boolean,
    /// Menu of named items.
    Menu,
    /// Action without a value.
    Button,
    /// 64-bit integer.
    Integer64,
    /// A control class marker (not a real control).
    CtrlClass,
    /// String.
    String,
    /// 32-bit bitmask.
    Bitmask,
    /// Menu of 64-bit integers (e.g. `V4L2_CID_LINK_FREQ`).
    IntegerMenu,
    /// Array of u8.
    U8,
    /// Array of u16.
    U16,
    /// Array of u32.
    U32,
    /// `struct v4l2_area`.
    Area,
    /// `struct v4l2_rect`.
    Rect,
    /// Any other (codec, HDR, ...) compound type.
    Other(u32),
}

impl ControlType {
    /// Converts a raw type.
    pub fn from_raw(v: u32) -> Self {
        use ControlType::*;
        match v {
            1 => Integer,
            2 => Boolean,
            3 => Menu,
            4 => Button,
            5 => Integer64,
            6 => CtrlClass,
            7 => String,
            8 => Bitmask,
            9 => IntegerMenu,
            0x100 => U8,
            0x101 => U16,
            0x102 => U32,
            0x106 => Area,
            0x107 => Rect,
            other => Other(other),
        }
    }

    /// True for compound (payload) types.
    pub fn is_compound(self) -> bool {
        matches!(
            self,
            ControlType::U8
                | ControlType::U16
                | ControlType::U32
                | ControlType::Area
                | ControlType::Rect
        ) || matches!(self, ControlType::Other(v) if v >= 0x100)
    }
}

flags! {
    /// Control flags (`V4L2_CTRL_FLAG_*`).
    pub struct ControlFlags: u32 {
        const DISABLED = 0x0001;
        const GRABBED = 0x0002;
        const READ_ONLY = 0x0004;
        const UPDATE = 0x0008;
        const INACTIVE = 0x0010;
        const SLIDER = 0x0020;
        const WRITE_ONLY = 0x0040;
        const VOLATILE = 0x0080;
        const HAS_PAYLOAD = 0x0100;
        const EXECUTE_ON_WRITE = 0x0200;
        const MODIFY_LAYOUT = 0x0400;
        const DYNAMIC_ARRAY = 0x0800;
        const HAS_WHICH_MIN_MAX = 0x1000;
    }
}

/// A control's description (`VIDIOC_QUERY_EXT_CTRL`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlInfo {
    /// Control id (`V4L2_CID_*`).
    pub id: u32,
    /// Type.
    pub control_type: ControlType,
    /// Name.
    pub name: String,
    /// Minimum value.
    pub minimum: i64,
    /// Maximum value.
    pub maximum: i64,
    /// Step.
    pub step: u64,
    /// Default value.
    pub default: i64,
    /// Flags.
    pub flags: ControlFlags,
    /// Size of one element in bytes.
    pub elem_size: u32,
    /// Number of elements (product of `dims`).
    pub elems: u32,
    /// Array dimensions (empty for scalars).
    pub dims: Vec<u32>,
}

impl ControlInfo {
    /// True when the value is passed by pointer (strings, arrays, compound types).
    pub fn has_payload(&self) -> bool {
        self.flags.contains(ControlFlags::HAS_PAYLOAD)
    }

    /// True for menu and integer-menu controls.
    pub fn is_menu(&self) -> bool {
        matches!(
            self.control_type,
            ControlType::Menu | ControlType::IntegerMenu
        )
    }

    /// True when the value can be read (not write-only, a button or a class marker).
    pub fn is_readable(&self) -> bool {
        !self.flags.contains(ControlFlags::WRITE_ONLY)
            && !matches!(
                self.control_type,
                ControlType::Button | ControlType::CtrlClass
            )
    }

    fn from_raw(raw: &raw::v4l2_query_ext_ctrl) -> Self {
        let nr = (raw.nr_of_dims as usize).min(raw::V4L2_CTRL_MAX_DIMS);
        Self {
            id: raw.id,
            control_type: ControlType::from_raw(raw.type_),
            name: cstr_field(&raw.name),
            minimum: raw.minimum,
            maximum: raw.maximum,
            step: raw.step,
            default: raw.default_value,
            flags: ControlFlags(raw.flags),
            elem_size: raw.elem_size,
            elems: raw.elems,
            dims: raw.dims[..nr].to_vec(),
        }
    }
}

/// The value of a menu item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuValue {
    /// A named item (`Menu`).
    Name(String),
    /// An integer item (`IntegerMenu`).
    Integer(i64),
}

/// One item of a menu control (`VIDIOC_QUERYMENU`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    /// Item index (the control value that selects it).
    pub index: u32,
    /// Item value.
    pub value: MenuValue,
}

/// A control value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlValue {
    /// Integer, boolean (0/1), menu index, bitmask or button (any value) controls.
    Integer(i32),
    /// 64-bit integer controls.
    Integer64(i64),
    /// String controls.
    String(String),
    /// Compound or array controls: the raw payload.
    Payload(Vec<u8>),
}

impl ControlValue {
    /// The value as an integer, for `Integer` and `Integer64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ControlValue::Integer(v) => Some(i64::from(*v)),
            ControlValue::Integer64(v) => Some(*v),
            _ => None,
        }
    }
}

/// Which value set a control operation reads or writes.
#[derive(Clone, Copy, Debug)]
pub enum ControlWhich<'fd> {
    /// The current values.
    Current,
    /// The default values (read-only).
    Default,
    /// The values stored in (or to be applied with) a media request.
    Request(BorrowedFd<'fd>),
    /// The minimum values (read-only; controls with `HAS_WHICH_MIN_MAX`).
    Minimum,
    /// The maximum values (read-only; controls with `HAS_WHICH_MIN_MAX`).
    Maximum,
}

impl ControlWhich<'_> {
    fn raw(self) -> (u32, i32) {
        match self {
            ControlWhich::Current => (WHICH_CUR_VAL, 0),
            ControlWhich::Default => (WHICH_DEF_VAL, 0),
            ControlWhich::Request(fd) => (WHICH_REQUEST_VAL, fd.as_raw_fd()),
            ControlWhich::Minimum => (WHICH_MIN_VAL, 0),
            ControlWhich::Maximum => (WHICH_MAX_VAL, 0),
        }
    }
}

/// Storage for one `v4l2_ext_control` and its payload during a call.
struct Slot {
    ctrl: raw::v4l2_ext_control,
    payload: Vec<u8>,
    kind: SlotKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Value,
    Value64,
    String,
    Payload,
}

impl Slot {
    fn for_get(info: &ControlInfo) -> Self {
        let mut ctrl: raw::v4l2_ext_control = zeroed();
        ctrl.id = info.id;
        let kind = match info.control_type {
            _ if info.has_payload() && info.control_type == ControlType::String => SlotKind::String,
            _ if info.has_payload() => SlotKind::Payload,
            ControlType::Integer64 => SlotKind::Value64,
            _ => SlotKind::Value,
        };
        let payload = match kind {
            // A string control's maximum is its maximum length, excluding the NUL.
            SlotKind::String => {
                vec![0u8; (info.maximum.max(0) as usize + 1) * info.elems.max(1) as usize]
            }
            SlotKind::Payload => vec![0u8; info.elem_size as usize * info.elems.max(1) as usize],
            _ => Vec::new(),
        };
        Self {
            ctrl,
            payload,
            kind,
        }
    }

    fn for_set(id: u32, value: &ControlValue) -> Self {
        let mut ctrl: raw::v4l2_ext_control = zeroed();
        ctrl.id = id;
        let (kind, payload) = match value {
            ControlValue::Integer(v) => {
                ctrl.u.value = *v;
                (SlotKind::Value, Vec::new())
            }
            ControlValue::Integer64(v) => {
                ctrl.u.value64 = *v;
                (SlotKind::Value64, Vec::new())
            }
            ControlValue::String(s) => {
                let mut bytes = s.as_bytes().to_vec();
                bytes.push(0);
                (SlotKind::String, bytes)
            }
            ControlValue::Payload(p) => (SlotKind::Payload, p.clone()),
        };
        Self {
            ctrl,
            payload,
            kind,
        }
    }

    /// Points the control at its payload buffer (after the slot has stopped moving).
    fn attach(&mut self) {
        if matches!(self.kind, SlotKind::String | SlotKind::Payload) {
            self.ctrl.size = self.payload.len() as u32;
            self.ctrl.u.ptr = self.payload.as_mut_ptr().cast();
        }
    }

    fn value(self) -> ControlValue {
        match self.kind {
            // SAFETY: the kernel wrote the `value` member for this non-payload control.
            SlotKind::Value => ControlValue::Integer(unsafe { self.ctrl.u.value }),
            // SAFETY: the kernel wrote the `value64` member for this 64-bit control.
            SlotKind::Value64 => ControlValue::Integer64(unsafe { self.ctrl.u.value64 }),
            SlotKind::String => ControlValue::String(cstr_field(&self.payload)),
            SlotKind::Payload => {
                let mut p = self.payload;
                // Dynamic arrays report the used size back in `size`.
                p.truncate(self.ctrl.size as usize);
                ControlValue::Payload(p)
            }
        }
    }
}

/// Runs an extended-controls ioctl over `slots`.
fn ext_ctrls(
    fd: BorrowedFd<'_>,
    req: ioctl::Ioctl,
    which: ControlWhich<'_>,
    slots: &mut [Slot],
) -> Result<()> {
    for slot in slots.iter_mut() {
        slot.attach();
    }
    let mut ctrls: Vec<raw::v4l2_ext_control> = slots.iter().map(|s| s.ctrl).collect();
    let (which, request_fd) = which.raw();
    let mut raw = raw::v4l2_ext_controls {
        which,
        count: ctrls.len() as u32,
        error_idx: 0,
        request_fd,
        reserved: [0],
        controls: ctrls.as_mut_ptr(),
    };
    // SAFETY: the ioctl takes a `v4l2_ext_controls` pointing to `count` controls in `ctrls`;
    // payload pointers point into `slots[i].payload`, which are not touched until it returns.
    let res = unsafe { ioctl::ioctl(fd, req, &mut raw) };
    if let Err(Error::Ioctl { name, errno }) = res {
        let idx = raw.error_idx as usize;
        return Err(match ctrls.get(idx) {
            Some(c) if idx < ctrls.len() => Error::Control {
                name,
                errno,
                control: c.id,
            },
            _ => Error::Ioctl { name, errno },
        });
    }
    res?;
    for (slot, ctrl) in slots.iter_mut().zip(&ctrls) {
        slot.ctrl = *ctrl;
    }
    Ok(())
}

/// The control API, shared by video nodes and subdevices.
pub trait Controls: AsFd {
    /// Describes one control (`VIDIOC_QUERY_EXT_CTRL`).
    fn query_control(&self, id: u32) -> Result<ControlInfo> {
        let mut raw: raw::v4l2_query_ext_ctrl = zeroed();
        raw.id = id;
        // SAFETY: VIDIOC_QUERY_EXT_CTRL takes a `v4l2_query_ext_ctrl`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_QUERY_EXT_CTRL, &mut raw)? };
        Ok(ControlInfo::from_raw(&raw))
    }

    /// Describes every control, including compound controls and class markers
    /// (`VIDIOC_QUERY_EXT_CTRL` with `NEXT_CTRL | NEXT_COMPOUND`).
    fn query_controls(&self) -> Result<Vec<ControlInfo>> {
        let mut out = Vec::new();
        let mut id = NEXT_CTRL | NEXT_COMPOUND;
        loop {
            let mut raw: raw::v4l2_query_ext_ctrl = zeroed();
            raw.id = id;
            // SAFETY: VIDIOC_QUERY_EXT_CTRL takes a `v4l2_query_ext_ctrl`.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_QUERY_EXT_CTRL, &mut raw) } {
                Ok(_) => {}
                Err(e) if e.is_invalid_argument() => break,
                Err(e) if e.is_not_supported() && out.is_empty() => break,
                Err(e) => return Err(e),
            }
            let info = ControlInfo::from_raw(&raw);
            id = info.id | NEXT_CTRL | NEXT_COMPOUND;
            out.push(info);
        }
        Ok(out)
    }

    /// Lists the items of a menu or integer-menu control (`VIDIOC_QUERYMENU`); indexes the
    /// driver skips are left out.
    fn query_menu(&self, info: &ControlInfo) -> Result<Vec<MenuItem>> {
        if !info.is_menu() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for index in info.minimum.max(0)..=info.maximum.max(-1) {
            let mut raw: raw::v4l2_querymenu = zeroed();
            raw.id = info.id;
            raw.index = index as u32;
            // SAFETY: VIDIOC_QUERYMENU takes a `v4l2_querymenu`.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_QUERYMENU, &mut raw) } {
                Ok(_) => {}
                Err(e) if e.is_invalid_argument() => continue,
                Err(e) => return Err(e),
            }
            let name = raw.name;
            let value = if info.control_type == ControlType::IntegerMenu {
                let mut v = [0u8; 8];
                v.copy_from_slice(&name[..8]);
                MenuValue::Integer(i64::from_ne_bytes(v))
            } else {
                MenuValue::Name(cstr_field(&name))
            };
            out.push(MenuItem {
                index: index as u32,
                value,
            });
        }
        Ok(out)
    }

    /// Reads one control's current value.
    fn control(&self, id: u32) -> Result<ControlValue> {
        let info = self.query_control(id)?;
        let mut values =
            self.get_controls_for(ControlWhich::Current, std::slice::from_ref(&info))?;
        Ok(values.remove(0))
    }

    /// Sets one control's current value.
    fn set_control(&self, id: u32, value: ControlValue) -> Result<()> {
        self.set_controls(ControlWhich::Current, &[(id, value)])
    }

    /// Reads several controls atomically (`VIDIOC_G_EXT_CTRLS`), querying each one's type first.
    fn get_controls(&self, which: ControlWhich<'_>, ids: &[u32]) -> Result<Vec<ControlValue>> {
        let infos = ids
            .iter()
            .map(|&id| self.query_control(id))
            .collect::<Result<Vec<_>>>()?;
        self.get_controls_for(which, &infos)
    }

    /// Reads several controls atomically (`VIDIOC_G_EXT_CTRLS`) whose descriptions are known.
    fn get_controls_for(
        &self,
        which: ControlWhich<'_>,
        infos: &[ControlInfo],
    ) -> Result<Vec<ControlValue>> {
        let mut slots: Vec<Slot> = infos.iter().map(Slot::for_get).collect();
        ext_ctrls(self.as_fd(), raw::VIDIOC_G_EXT_CTRLS, which, &mut slots)?;
        Ok(slots.into_iter().map(Slot::value).collect())
    }

    /// Sets several controls atomically (`VIDIOC_S_EXT_CTRLS`). With
    /// [`ControlWhich::Request`], the values are stored in the request and applied when it is
    /// queued.
    fn set_controls(&self, which: ControlWhich<'_>, values: &[(u32, ControlValue)]) -> Result<()> {
        let mut slots: Vec<Slot> = values.iter().map(|(id, v)| Slot::for_set(*id, v)).collect();
        ext_ctrls(self.as_fd(), raw::VIDIOC_S_EXT_CTRLS, which, &mut slots)
    }

    /// Checks whether values would be accepted, without applying them
    /// (`VIDIOC_TRY_EXT_CTRLS`).
    fn try_controls(&self, which: ControlWhich<'_>, values: &[(u32, ControlValue)]) -> Result<()> {
        let mut slots: Vec<Slot> = values.iter().map(|(id, v)| Slot::for_set(*id, v)).collect();
        ext_ctrls(self.as_fd(), raw::VIDIOC_TRY_EXT_CTRLS, which, &mut slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        control_type: ControlType,
        flags: ControlFlags,
        maximum: i64,
        elem_size: u32,
        elems: u32,
    ) -> ControlInfo {
        ControlInfo {
            id: 0x0098_0900,
            control_type,
            name: "test".into(),
            minimum: 0,
            maximum,
            step: 1,
            default: 0,
            flags,
            elem_size,
            elems,
            dims: Vec::new(),
        }
    }

    #[test]
    fn slots_pick_the_right_union_member() {
        let s = Slot::for_get(&info(ControlType::Integer, ControlFlags::empty(), 10, 4, 1));
        assert!(s.kind == SlotKind::Value && s.payload.is_empty());
        let s = Slot::for_get(&info(
            ControlType::Integer64,
            ControlFlags::empty(),
            10,
            8,
            1,
        ));
        assert!(s.kind == SlotKind::Value64);
        let s = Slot::for_get(&info(
            ControlType::String,
            ControlFlags::HAS_PAYLOAD,
            31,
            32,
            1,
        ));
        assert!(s.kind == SlotKind::String && s.payload.len() == 32);
        let mut s = Slot::for_get(&info(ControlType::U16, ControlFlags::HAS_PAYLOAD, 0, 2, 12));
        assert!(s.kind == SlotKind::Payload && s.payload.len() == 24);
        s.attach();
        assert_eq!({ s.ctrl.size }, 24);
    }

    #[test]
    fn set_slots_carry_values() {
        let s = Slot::for_set(1, &ControlValue::Integer(-5));
        assert_eq!(s.value(), ControlValue::Integer(-5));
        let s = Slot::for_set(1, &ControlValue::Integer64(1 << 40));
        assert_eq!(s.value(), ControlValue::Integer64(1 << 40));
        let mut s = Slot::for_set(1, &ControlValue::String("abc".into()));
        s.attach();
        assert_eq!({ s.ctrl.size }, 4);
        assert_eq!(s.value(), ControlValue::String("abc".into()));
    }

    #[test]
    fn control_types_decode() {
        assert_eq!(ControlType::from_raw(9), ControlType::IntegerMenu);
        assert!(ControlType::from_raw(0x107).is_compound());
        assert!(ControlType::from_raw(0x200).is_compound());
        assert!(!ControlType::Integer.is_compound());
    }
}

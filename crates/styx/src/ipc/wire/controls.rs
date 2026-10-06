//! Control messages of a camera service (new kinds; the protocol version is unchanged: only
//! clients that ask send them, and the service sends events only on connections that
//! subscribed, so older peers never see them).
//!
//! Client to server, `Control`: a sequence number (`u32`, echoed in the reply), the camera
//! (optional text), the client's token from its accept message (optional `u64`), then the
//! operation: 0 set (target, value), 1 get (target), 2 list, 3 subscribe. A target is 0 and a
//! control id (`u32`) or 1 and a standard control (`u8`).
//!
//! Server to client, `ControlReply`: the sequence number, then 0 applied (id, requested and
//! effective values, clamped, frame (optional `u64`), deferred, restarted), 1 a value (id,
//! value), 2 a list (count `u16`, length `u64`; the descriptors in the attached memfd, read
//! with [`decode_control_list`]), 3 refused (reason code, text), 4 subscribed.
//!
//! Server to client, `ControlEvent`: id, standard control (optional `u8`), value, frame
//! (optional `u64`), the client that changed it (optional `u64`).
//!
//! A value is a tag and its payload: 0 none, 1 bool, 2 `i32`, 3 `u32`, 4 `f32` bits, 5 a
//! rectangle (`i32` x, y, `u32` width, height), 6 up to 16 rectangles.

use styx_core::prelude::*;

use super::{IpcError, Reader, Writer};
use crate::ipc::controls::{
    AppliedControl, ControlDescriptor, ControlEvent, ControlRefusal, ControlTarget, StandardControl,
};

pub(super) const KIND_CONTROL: u16 = 11;
pub(super) const KIND_CONTROL_REPLY: u16 = 12;
pub(super) const KIND_CONTROL_EVENT: u16 = 13;

/// Most controls a list carries, rectangles a value, entries a menu.
const MAX_CONTROLS: usize = 512;
const MAX_RECTS: usize = 16;
const MAX_MENU: usize = 64;
/// Longest control list accepted (in its memfd).
pub(in crate::ipc) const MAX_CONTROL_LIST: usize = 1 << 20;

/// What a client asks of a camera's controls.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ipc) enum ControlOp {
    Set(ControlTarget, ControlValue),
    Get(ControlTarget),
    List,
    Subscribe,
}

/// A control request: its sequence number, the camera, the client's token, the operation.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ipc) struct ControlRequest {
    pub seq: u32,
    pub camera: Option<String>,
    pub token: Option<u64>,
    pub op: ControlOp,
}

/// The service's answer to a control request.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ipc) enum ControlReply {
    Applied(AppliedControl),
    Value(ControlId, ControlValue),
    /// This many descriptors, this many bytes, in the attached memfd.
    List(u16, usize),
    Refused(ControlRefusal),
    Subscribed,
}

fn write_value(w: &mut Writer, v: &ControlValue) {
    let rect = |w: &mut Writer, r: &ControlRect| {
        w.u32(r.x as u32);
        w.u32(r.y as u32);
        w.u32(r.width);
        w.u32(r.height);
    };
    match v {
        ControlValue::None => w.u8(0),
        ControlValue::Bool(b) => {
            w.u8(1);
            w.bool(*b);
        }
        ControlValue::Int(i) => {
            w.u8(2);
            w.u32(*i as u32);
        }
        ControlValue::Uint(u) => {
            w.u8(3);
            w.u32(*u);
        }
        ControlValue::Float(f) => {
            w.u8(4);
            w.u32(f.to_bits());
        }
        ControlValue::Rect(r) => {
            w.u8(5);
            rect(w, r);
        }
        ControlValue::Rects(rs) => {
            w.u8(6);
            let rs = &rs[..rs.len().min(MAX_RECTS)];
            w.u8(rs.len() as u8);
            for r in rs {
                rect(w, r);
            }
        }
    }
}

fn read_value(r: &mut Reader<'_>) -> Result<ControlValue, IpcError> {
    let rect = |r: &mut Reader<'_>| -> Result<ControlRect, IpcError> {
        Ok(ControlRect {
            x: r.u32()? as i32,
            y: r.u32()? as i32,
            width: r.u32()?,
            height: r.u32()?,
        })
    };
    Ok(match r.u8()? {
        0 => ControlValue::None,
        1 => ControlValue::Bool(r.bool()?),
        2 => ControlValue::Int(r.u32()? as i32),
        3 => ControlValue::Uint(r.u32()?),
        4 => ControlValue::Float(f32::from_bits(r.u32()?)),
        5 => ControlValue::Rect(rect(r)?),
        6 => {
            let count = usize::from(r.u8()?);
            if count > MAX_RECTS {
                return Err(IpcError::Malformed("too many rectangles"));
            }
            ControlValue::Rects((0..count).map(|_| rect(r)).collect::<Result<_, _>>()?)
        }
        _ => return Err(IpcError::Malformed("unknown control value")),
    })
}

fn write_target(w: &mut Writer, t: ControlTarget) {
    match t {
        ControlTarget::Id(id) => {
            w.u8(0);
            w.u32(id.0);
        }
        ControlTarget::Standard(s) => {
            w.u8(1);
            w.u8(s as u8);
        }
    }
}

fn read_standard(r: &mut Reader<'_>) -> Result<StandardControl, IpcError> {
    StandardControl::from_code(r.u8()?).ok_or(IpcError::Malformed("unknown standard control"))
}

fn read_target(r: &mut Reader<'_>) -> Result<ControlTarget, IpcError> {
    match r.u8()? {
        0 => Ok(ControlTarget::Id(ControlId(r.u32()?))),
        1 => Ok(ControlTarget::Standard(read_standard(r)?)),
        _ => Err(IpcError::Malformed("unknown control target")),
    }
}

pub(in crate::ipc) fn encode_control(request: &ControlRequest) -> Vec<u8> {
    let mut w = Writer::new(KIND_CONTROL);
    w.u32(request.seq);
    w.opt(request.camera.as_deref(), Writer::text);
    w.opt(request.token, Writer::u64);
    match &request.op {
        ControlOp::Set(target, value) => {
            w.u8(0);
            write_target(&mut w, *target);
            write_value(&mut w, value);
        }
        ControlOp::Get(target) => {
            w.u8(1);
            write_target(&mut w, *target);
        }
        ControlOp::List => w.u8(2),
        ControlOp::Subscribe => w.u8(3),
    }
    w.0
}

pub(super) fn read_control(r: &mut Reader<'_>) -> Result<ControlRequest, IpcError> {
    let seq = r.u32()?;
    let camera = r.opt(Reader::text)?;
    let token = r.opt(Reader::u64)?;
    let op = match r.u8()? {
        0 => ControlOp::Set(read_target(r)?, read_value(r)?),
        1 => ControlOp::Get(read_target(r)?),
        2 => ControlOp::List,
        3 => ControlOp::Subscribe,
        _ => return Err(IpcError::Malformed("unknown control operation")),
    };
    Ok(ControlRequest {
        seq,
        camera,
        token,
        op,
    })
}

fn refusal_code(refusal: &ControlRefusal) -> (u8, &str) {
    match refusal {
        ControlRefusal::Unsupported(t) => (0, t),
        ControlRefusal::ReadOnly(t) => (1, t),
        ControlRefusal::Invalid(t) => (2, t),
        ControlRefusal::NotPermitted(t) => (3, t),
        ControlRefusal::Failed(t) => (4, t),
    }
}

pub(in crate::ipc) fn encode_control_reply(seq: u32, reply: &ControlReply) -> Vec<u8> {
    let mut w = Writer::new(KIND_CONTROL_REPLY);
    w.u32(seq);
    match reply {
        ControlReply::Applied(a) => {
            w.u8(0);
            w.u32(a.id.0);
            write_value(&mut w, &a.requested);
            write_value(&mut w, &a.value);
            w.bool(a.clamped);
            w.opt(a.frame, Writer::u64);
            w.bool(a.deferred);
            w.bool(a.restarted);
        }
        ControlReply::Value(id, value) => {
            w.u8(1);
            w.u32(id.0);
            write_value(&mut w, value);
        }
        ControlReply::List(count, len) => {
            w.u8(2);
            w.u16(*count);
            w.usize(*len);
        }
        ControlReply::Refused(refusal) => {
            w.u8(3);
            let (code, text) = refusal_code(refusal);
            w.u8(code);
            w.text(text);
        }
        ControlReply::Subscribed => w.u8(4),
    }
    w.0
}

pub(super) fn read_control_reply(r: &mut Reader<'_>) -> Result<(u32, ControlReply), IpcError> {
    let seq = r.u32()?;
    let reply = match r.u8()? {
        0 => ControlReply::Applied(AppliedControl {
            id: ControlId(r.u32()?),
            requested: read_value(r)?,
            value: read_value(r)?,
            clamped: r.bool()?,
            frame: r.opt(Reader::u64)?,
            deferred: r.bool()?,
            restarted: r.bool()?,
        }),
        1 => ControlReply::Value(ControlId(r.u32()?), read_value(r)?),
        2 => {
            let count = r.u16()?;
            let len = r.usize()?;
            if usize::from(count) > MAX_CONTROLS || len > MAX_CONTROL_LIST {
                return Err(IpcError::Malformed("control list too long"));
            }
            ControlReply::List(count, len)
        }
        3 => {
            let code = r.u8()?;
            let text = r.text()?;
            ControlReply::Refused(match code {
                0 => ControlRefusal::Unsupported(text),
                1 => ControlRefusal::ReadOnly(text),
                2 => ControlRefusal::Invalid(text),
                3 => ControlRefusal::NotPermitted(text),
                4 => ControlRefusal::Failed(text),
                _ => return Err(IpcError::Malformed("unknown control refusal")),
            })
        }
        4 => ControlReply::Subscribed,
        _ => return Err(IpcError::Malformed("unknown control reply")),
    };
    Ok((seq, reply))
}

pub(in crate::ipc) fn encode_control_event(event: &ControlEvent) -> Vec<u8> {
    let mut w = Writer::new(KIND_CONTROL_EVENT);
    w.u32(event.id.0);
    w.opt(event.standard, |w, s| w.u8(s as u8));
    write_value(&mut w, &event.value);
    w.opt(event.frame, Writer::u64);
    w.opt(event.by, Writer::u64);
    w.0
}

pub(super) fn read_control_event(r: &mut Reader<'_>) -> Result<ControlEvent, IpcError> {
    Ok(ControlEvent {
        id: ControlId(r.u32()?),
        standard: r.opt(read_standard)?,
        value: read_value(r)?,
        frame: r.opt(Reader::u64)?,
        by: r.opt(Reader::u64)?,
    })
}

fn kind_code(kind: ControlKind) -> u8 {
    match kind {
        ControlKind::None => 0,
        ControlKind::Bool => 1,
        ControlKind::Rectangle => 2,
        ControlKind::Int => 3,
        ControlKind::Uint => 4,
        ControlKind::Float => 5,
        ControlKind::Menu => 6,
        ControlKind::IntMenu => 7,
        ControlKind::Unknown => 8,
    }
}

fn kind_from(code: u8) -> ControlKind {
    match code {
        0 => ControlKind::None,
        1 => ControlKind::Bool,
        2 => ControlKind::Rectangle,
        3 => ControlKind::Int,
        4 => ControlKind::Uint,
        5 => ControlKind::Float,
        6 => ControlKind::Menu,
        7 => ControlKind::IntMenu,
        _ => ControlKind::Unknown,
    }
}

/// A control list (the body of a list reply's memfd), at most [`MAX_CONTROLS`] of them.
pub(in crate::ipc) fn encode_control_list(controls: &[ControlDescriptor]) -> Vec<u8> {
    let mut w = Writer(Vec::with_capacity(64 * controls.len()));
    let controls = &controls[..controls.len().min(MAX_CONTROLS)];
    w.u16(controls.len() as u16);
    for c in controls {
        let m = &c.meta;
        w.u32(m.id.0);
        w.text(&m.name);
        w.u8(kind_code(m.kind));
        w.bool(m.access == Access::ReadWrite);
        write_value(&mut w, &m.min);
        write_value(&mut w, &m.max);
        write_value(&mut w, &m.default);
        w.opt(m.step.as_ref(), write_value);
        w.opt(m.menu.as_ref(), |w, menu| {
            let menu = &menu[..menu.len().min(MAX_MENU)];
            w.u8(menu.len() as u8);
            for entry in menu {
                w.text(entry);
            }
        });
        w.opt(c.current.as_ref(), write_value);
        w.opt(c.standard, |w, s| w.u8(s as u8));
        w.bool(c.writable);
    }
    w.0
}

/// The descriptors of a list reply's memfd (another process wrote them).
pub(in crate::ipc) fn decode_control_list(
    bytes: &[u8],
) -> Result<Vec<ControlDescriptor>, IpcError> {
    if bytes.len() > MAX_CONTROL_LIST {
        return Err(IpcError::Malformed("control list too long"));
    }
    let mut r = Reader(bytes);
    let count = usize::from(r.u16()?);
    if count > MAX_CONTROLS {
        return Err(IpcError::Malformed("too many controls"));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let id = ControlId(r.u32()?);
        let name = r.text()?;
        let kind = kind_from(r.u8()?);
        let access = if r.bool()? {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        };
        let (min, max, default) = (
            read_value(&mut r)?,
            read_value(&mut r)?,
            read_value(&mut r)?,
        );
        let step = r.opt(read_value)?;
        let menu = r.opt(|r| {
            let n = usize::from(r.u8()?);
            if n > MAX_MENU {
                return Err(IpcError::Malformed("menu too long"));
            }
            (0..n).map(|_| r.text()).collect::<Result<Vec<_>, _>>()
        })?;
        out.push(ControlDescriptor {
            meta: ControlMeta {
                id,
                name,
                kind,
                access,
                min,
                max,
                default,
                step,
                menu,
                metadata: ControlMetadata::default(),
            },
            current: r.opt(read_value)?,
            standard: r.opt(read_standard)?,
            writable: r.bool()?,
        });
    }
    Ok(out)
}

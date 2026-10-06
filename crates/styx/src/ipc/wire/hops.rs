//! The hops trailer of frame and release messages: appended after a message's fields, so a
//! peer that predates it ignores it (decoders do not check for trailing bytes) and a message
//! without it reads as one without hops. The protocol version is unchanged.
//!
//! Frame (server to client): tag 1, a byte with a bit per hop present (bit `i`:
//! `Hop::ALL[i]`), a `u64` per hop present (`CLOCK_MONOTONIC` ns), the sequence number
//! (optional `u32`), the copies (`u32`) and their bytes (`u64`).
//!
//! Release (client to server): tag 1, the client's receive and import times (`u64` ns each,
//! 0: unknown).

use styx_core::prelude::{FrameHops, Hop};

use super::{IpcError, Reader, Writer};

const TAG: u8 = 1;

/// A client's receive and import times of a frame it released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ipc) struct ClientHops {
    pub received: Option<u64>,
    pub imported: Option<u64>,
}

pub(super) fn write_frame_hops(w: &mut Writer, hops: &FrameHops) {
    if hops.is_empty() {
        return;
    }
    w.u8(TAG);
    let mask = hops.iter().fold(0u8, |m, (h, _)| m | 1 << h.index());
    w.u8(mask);
    for (_, ns) in hops.iter() {
        w.u64(ns);
    }
    w.opt(hops.sequence(), Writer::u32);
    w.u32(hops.copies());
    w.u64(hops.copied_bytes());
}

/// The trailer, if there is one (a frame from a server that sends none reads as `None`).
pub(super) fn read_frame_hops(r: &mut Reader<'_>) -> Result<Option<FrameHops>, IpcError> {
    if r.0.is_empty() {
        return Ok(None);
    }
    if r.u8()? != TAG {
        return Ok(None);
    }
    let mask = r.u8()?;
    let mut hops = FrameHops::new();
    for hop in Hop::ALL {
        if mask & 1 << hop.index() != 0 {
            hops.set(hop, r.u64()?);
        }
    }
    hops.set_sequence(r.opt(Reader::u32)?);
    let copies = r.u32()?;
    hops.set_copies(copies, r.u64()?);
    Ok(Some(hops))
}

pub(super) fn read_release_hops(r: &mut Reader<'_>) -> Result<Option<ClientHops>, IpcError> {
    if r.0.is_empty() || r.u8()? != TAG {
        return Ok(None);
    }
    let at = |v: u64| (v != 0).then_some(v);
    Ok(Some(ClientHops {
        received: at(r.u64()?),
        imported: at(r.u64()?),
    }))
}

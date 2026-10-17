//! The sliced-frame trailers (`progress_ring.rs`), after a message's other trailers, so peers
//! that predate them ignore them; the protocol version is unchanged.
//!
//! Request (client to camera service): tag 2, then 1: the client takes frames before they are
//! complete. Without it (older clients) a sliced frame is sent once complete.
//!
//! Frame (camera service to such a client): tag 2, the frame's slot in the connection's
//! progress page (`u8`), each output's height (`u32` x 2), whether the page's descriptor is
//! attached after the frame's own (`u8`: the first sliced frame of a connection), and the output
//! each of the frame's leases is, the frame's own first then its companions' in order (a count
//! `u8`, then an `u8` each; 255: complete, no progress).

use smallvec::SmallVec;

use super::{IpcError, Reader, Writer};

const TAG: u8 = 2;

/// Output index of a lease without progress.
pub(in crate::ipc) const NO_OUTPUT: u8 = u8::MAX;

/// A sliced frame's progress trailer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::ipc) struct WireProgress {
    pub slot: u8,
    pub heights: [u32; 2],
    /// The connection's progress page follows the frame's descriptors.
    pub ring_attached: bool,
    /// The output of the frame's lease, then of each companion ([`NO_OUTPUT`]: none).
    pub outputs: SmallVec<[u8; 4]>,
}

pub(super) fn write_slices_wanted(w: &mut Writer) {
    w.u8(TAG);
    w.u8(1);
}

/// Whether a request's trailers ask for frames before they are complete.
pub(super) fn read_slices_wanted(r: &mut Reader<'_>) -> Result<bool, IpcError> {
    if r.0.first() != Some(&TAG) {
        return Ok(false);
    }
    r.u8()?;
    Ok(r.u8()? != 0)
}

pub(super) fn write_frame_progress(w: &mut Writer, p: &WireProgress) {
    w.u8(TAG);
    w.u8(p.slot);
    w.u32(p.heights[0]);
    w.u32(p.heights[1]);
    w.u8(u8::from(p.ring_attached));
    let n = p.outputs.len().min(usize::from(u8::MAX));
    w.u8(n as u8);
    for &o in p.outputs.iter().take(n) {
        w.u8(o);
    }
}

/// The trailer, if the frame has one.
pub(super) fn read_frame_progress(r: &mut Reader<'_>) -> Result<Option<WireProgress>, IpcError> {
    if r.0.first() != Some(&TAG) {
        return Ok(None);
    }
    r.u8()?;
    let slot = r.u8()?;
    let heights = [r.u32()?, r.u32()?];
    let ring_attached = r.u8()? != 0;
    let n = r.u8()?;
    let mut outputs = SmallVec::new();
    for _ in 0..n {
        outputs.push(r.u8()?);
    }
    Ok(Some(WireProgress {
        slot,
        heights,
        ring_attached,
        outputs,
    }))
}

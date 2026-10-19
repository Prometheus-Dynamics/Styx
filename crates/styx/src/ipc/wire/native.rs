//! The native sensor trailer: a frame's `NativeFrameMeta` (exposure, gains, frame timing, end
//! of frame, and what the 3A loop knew of it), after the other trailers, so peers that predate
//! it ignore it; the protocol version is unchanged.
//!
//! Tag 3, then the fields in order; floats as their bits. The 3A record and its lens follow a
//! presence byte each; a lens's `settled` is 0 unknown, 1 no, 2 yes.

use styx_core::prelude::{NativeAaa, NativeAfState, NativeFrameMeta, NativeLens};

use super::{IpcError, Reader, Writer};

const TAG: u8 = 3;

pub(super) fn write_native(w: &mut Writer, n: &NativeFrameMeta) {
    w.u8(TAG);
    w.u32(n.sequence);
    w.u32(n.bytes_used);
    w.bool(n.error);
    w.u64(n.exposure_ns);
    w.u32(n.analog_gain.to_bits());
    w.u32(n.digital_gain.to_bits());
    w.u64(n.frame_duration_ns);
    w.u32(n.frame_length);
    w.bool(n.verified);
    w.u64(n.frame_end_ns);
    let Some(a) = &n.aaa else {
        w.u8(0);
        return;
    };
    w.u8(1);
    for g in a.colour_gains {
        w.u32(g.to_bits());
    }
    w.u32(a.colour_temperature_k.to_bits());
    w.u32(a.lux.to_bits());
    w.bool(a.ae_locked);
    w.bool(a.awb_converged);
    w.u64(a.flicker_period_ns);
    match &a.lens {
        None => w.u8(0),
        Some(l) => {
            w.u8(1);
            w.u32(l.position_dioptres.to_bits());
            w.u8(l.af_state.tag());
            w.u8(match l.settled {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            });
        }
    }
}

/// The trailer, if the frame has one.
pub(super) fn read_native(r: &mut Reader<'_>) -> Result<Option<NativeFrameMeta>, IpcError> {
    if r.0.first() != Some(&TAG) {
        return Ok(None);
    }
    r.u8()?;
    let f32_ = |r: &mut Reader<'_>| r.u32().map(f32::from_bits);
    let mut n = NativeFrameMeta {
        sequence: r.u32()?,
        bytes_used: r.u32()?,
        error: r.bool()?,
        exposure_ns: r.u64()?,
        analog_gain: f32_(r)?,
        digital_gain: f32_(r)?,
        frame_duration_ns: r.u64()?,
        frame_length: r.u32()?,
        verified: r.bool()?,
        frame_end_ns: r.u64()?,
        aaa: None,
    };
    if r.u8()? != 0 {
        let colour_gains = [f32_(r)?, f32_(r)?, f32_(r)?];
        let mut a = NativeAaa {
            colour_gains,
            colour_temperature_k: f32_(r)?,
            lux: f32_(r)?,
            ae_locked: r.bool()?,
            awb_converged: r.bool()?,
            flicker_period_ns: r.u64()?,
            lens: None,
        };
        if r.u8()? != 0 {
            a.lens = Some(NativeLens {
                position_dioptres: f32_(r)?,
                af_state: NativeAfState::from_tag(r.u8()?),
                settled: match r.u8()? {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                },
            });
        }
        n.aaa = Some(a);
    }
    Ok(Some(n))
}

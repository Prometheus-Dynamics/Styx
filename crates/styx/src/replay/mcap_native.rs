//! The end of frame and 3A record of a native frame in `styx/msg/FrameMeta` (after the native
//! sensor values; absent in earlier version 2 recordings).

use styx_core::prelude::{NativeAaa, NativeAfState, NativeFrameMeta, NativeLens};

use super::ReplayError;
use super::cdr::{CdrReader, CdrWriter};

pub(super) fn encode_aaa(w: &mut CdrWriter, n: &NativeFrameMeta) {
    w.u64(n.frame_end_ns);
    let a = n.aaa.unwrap_or_default();
    w.bool(n.aaa.is_some());
    for g in a.colour_gains {
        w.f32(g);
    }
    w.f32(a.colour_temperature_k);
    w.f32(a.lux);
    w.bool(a.ae_locked);
    w.bool(a.awb_converged);
    w.u64(a.flicker_period_ns);
    w.bool(a.lens.is_some());
    let lens = a.lens.unwrap_or(NativeLens {
        position_dioptres: 0.0,
        af_state: NativeAfState::Idle,
        settled: None,
    });
    w.f32(lens.position_dioptres);
    w.u8(lens.af_state.tag());
    w.u8(match lens.settled {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    });
}

/// The end of frame and 3A record of a native frame's metadata.
pub(super) fn decode_aaa(r: &mut CdrReader<'_>) -> Result<(u64, Option<NativeAaa>), ReplayError> {
    let frame_end_ns = r.u64()?;
    let valid = r.bool()?;
    let colour_gains = [r.f32()?, r.f32()?, r.f32()?];
    let (colour_temperature_k, lux) = (r.f32()?, r.f32()?);
    let (ae_locked, awb_converged, flicker_period_ns) = (r.bool()?, r.bool()?, r.u64()?);
    let lens_valid = r.bool()?;
    let (position_dioptres, af, settled) = (r.f32()?, r.u8()?, r.u8()?);
    let lens = lens_valid.then(|| NativeLens {
        position_dioptres,
        af_state: NativeAfState::from_tag(af),
        settled: match settled {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        },
    });
    let aaa = valid.then_some(NativeAaa {
        colour_gains,
        colour_temperature_k,
        lux,
        ae_locked,
        awb_converged,
        flicker_period_ns,
        lens,
    });
    Ok((frame_end_ns, aaa))
}

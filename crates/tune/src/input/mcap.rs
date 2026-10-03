//! Styx MCAP recordings (`styx::replay`): raw Bayer frames with the exposure and gains each was
//! made with (`NativeFrameMeta`, recording format 2), as `styx-tune capture` and
//! `StreamRecorder` write them.

use std::path::Path;

use styx::prelude::*;

use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// A Styx frame as a raw frame, if it is raw Bayer.
pub fn raw_frame(frame: &FrameLease) -> Option<RawFrame> {
    let f = frame.meta().format;
    let (w, h) = (f.resolution.width.get(), f.resolution.height.get());
    let format = styx_softisp::RawFormat::from_fourcc(f.code, w, h)?;
    let planes = frame.planes();
    let plane = planes.first()?;
    let (w, h) = (w as usize, h as usize);
    let need = format.packing.row_bytes(w);
    if plane.stride() < need || plane.data().len() < plane.stride() * (h - 1) + need {
        return None;
    }
    let mut data = vec![0u16; w * h];
    for (y, row) in data.chunks_exact_mut(w).enumerate() {
        styx_pipeline::rawrec::unpack_row(
            format.packing,
            &plane.data()[y * plane.stride()..],
            w,
            row,
        );
    }
    let native = frame.meta().native().copied();
    Some(RawFrame {
        width: w,
        height: h,
        cfa: Cfa::from_softisp(format.pattern),
        bits: format.packing.bit_depth(),
        data,
        exposure_us: native.map_or(0.0, |m| m.exposure_ns as f64 / 1e3),
        analogue_gain: native.map_or(1.0, |m| f64::from(m.analog_gain)),
        digital_gain: native.map_or(1.0, |m| f64::from(m.digital_gain)),
        black_level: None,
    })
}

/// Every raw frame of the recording.
pub fn load(path: &Path) -> Result<Vec<RawFrame>> {
    let fail = |e: &dyn std::fmt::Display| TuneError::Format(format!("{}: {e}", path.display()));
    let (header, frames) = styx::replay::open_recording(path).map_err(|e| fail(&e))?;
    let mut out = Vec::new();
    for frame in frames {
        let frame = frame.map_err(|e| fail(&e))?;
        out.push(raw_frame(&frame).ok_or_else(|| fail(&"not a raw Bayer frame"))?);
    }
    if out.is_empty() {
        return Err(fail(&format!(
            "no frames (recorded from {})",
            header.device.display
        )));
    }
    Ok(out)
}

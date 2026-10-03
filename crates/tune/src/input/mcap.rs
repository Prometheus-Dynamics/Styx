//! Styx MCAP recordings (`styx::replay`): raw Bayer frames with the exposure and gains each was
//! made with (`NativeFrameMeta`, recording format 2), as `styx-tune capture` and
//! `StreamRecorder` write them.

use std::path::Path;

use styx::prelude::*;

use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// Every raw frame of the recording.
pub fn load(path: &Path) -> Result<Vec<RawFrame>> {
    let fail = |e: &dyn std::fmt::Display| TuneError::Format(format!("{}: {e}", path.display()));
    let (header, frames) = styx::replay::open_recording(path).map_err(|e| fail(&e))?;
    let mut out = Vec::new();
    for frame in frames {
        let frame = frame.map_err(|e| fail(&e))?;
        let f = frame.meta().format;
        let (w, h) = (f.resolution.width.get(), f.resolution.height.get());
        let format = styx_softisp::RawFormat::from_fourcc(f.code, w, h)
            .ok_or_else(|| fail(&format!("{} is not a raw Bayer format", f.code)))?;
        let planes = frame.planes();
        let plane = planes.first().ok_or_else(|| fail(&"frame without data"))?;
        let (w, h) = (w as usize, h as usize);
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
        out.push(RawFrame {
            width: w,
            height: h,
            cfa: Cfa::from_softisp(format.pattern),
            bits: format.packing.bit_depth(),
            data,
            exposure_us: native.map_or(0.0, |m| m.exposure_ns as f64 / 1e3),
            analogue_gain: native.map_or(1.0, |m| f64::from(m.analog_gain)),
            digital_gain: native.map_or(1.0, |m| f64::from(m.digital_gain)),
            black_level: None,
        });
    }
    if out.is_empty() {
        return Err(fail(&format!(
            "no frames (recorded from {})",
            header.device.display
        )));
    }
    Ok(out)
}

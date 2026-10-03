//! Styx raw recordings (`styx-pipeline::rawrec`): `<base>.jsonl` (header and one line per frame
//! with its exposure and gains) and `<base>.raw` (the frames, packed as the sensor sent them).

use std::path::Path;

use styx_pipeline::rawrec::{RawRecording, unpack_row};

use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// Every frame of the recording at `path` (`.jsonl`, `.raw` or the base name).
pub fn load(path: &Path) -> Result<Vec<RawFrame>> {
    let base = path.with_extension("");
    let rec = RawRecording::open(&base)
        .map_err(|e| TuneError::Format(format!("{}: {e}", base.display())))?;
    let h = &rec.header;
    let (w, rows) = (h.format.width as usize, h.format.height as usize);
    let bits = h.format.packing.bit_depth();
    let cfa = Cfa::from_softisp(h.format.pattern);
    let mut out = Vec::with_capacity(rec.len());
    for (i, f) in rec.frames.iter().enumerate() {
        let bytes = rec.frame(i);
        let mut data = vec![0u16; w * rows];
        for (y, row) in data.chunks_exact_mut(w).enumerate() {
            unpack_row(h.format.packing, &bytes[y * h.stride..], w, row);
        }
        out.push(RawFrame {
            width: w,
            height: rows,
            cfa,
            bits,
            data,
            exposure_us: f.sensor.exposure.as_secs_f64() * 1e6,
            analogue_gain: f.sensor.analogue_gain,
            digital_gain: f.sensor.digital_gain,
            black_level: Some(h.sensor.black_level),
        });
    }
    Ok(out)
}

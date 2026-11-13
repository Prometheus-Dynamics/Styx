//! Reading raw captures.
//!
//! A raw file is consecutive frames of packed little-endian 16-bit samples, `width × height`
//! per frame, the bit depth in the session says how many low bits carry data. A raw file holds
//! no metadata, so the session states the size, the mosaic and the settings of its frames
//! ([`RawSpec`]).

use std::path::Path;

use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// What a raw file's frames are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawSpec {
    /// Samples across.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// The mosaic.
    pub cfa: Cfa,
    /// Bits per sample (8 to 16).
    pub bits: u8,
    /// Exposure of the frames, microseconds.
    pub exposure_us: f64,
    /// Analogue gain of the frames.
    pub gain: f64,
}

/// Every frame in the raw file at `path`, as `spec` describes them.
pub fn read_raw(path: &Path, spec: &RawSpec) -> Result<Vec<RawFrame>> {
    let bytes = std::fs::read(path).map_err(|e| TuneError::io(path, e))?;
    decode_raw(&bytes, spec).map_err(|e| TuneError::Format(format!("{}: {e}", path.display())))
}

/// The frames in `bytes` (see the module docs).
pub fn decode_raw(bytes: &[u8], spec: &RawSpec) -> std::result::Result<Vec<RawFrame>, String> {
    if spec.width == 0 || spec.height == 0 {
        return Err("empty frame size".into());
    }
    if !(8..=16).contains(&spec.bits) {
        return Err(format!("bit depth {} not in 8..=16", spec.bits));
    }
    let per_frame = spec.width * spec.height;
    let frame_bytes = per_frame * 2;
    if bytes.is_empty() || !bytes.len().is_multiple_of(frame_bytes) {
        return Err(format!(
            "{} bytes is not a whole number of {}x{} frames",
            bytes.len(),
            spec.width,
            spec.height
        ));
    }
    let mask = ((1u32 << spec.bits) - 1) as u16;
    Ok(bytes
        .chunks_exact(frame_bytes)
        .map(|chunk| RawFrame {
            width: spec.width,
            height: spec.height,
            cfa: spec.cfa,
            bits: spec.bits,
            data: chunk
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b) & mask)
                .collect(),
            exposure_us: spec.exposure_us,
            analogue_gain: spec.gain,
            digital_gain: 1.0,
            black_level: None,
        })
        .collect())
}

/// The raw bytes of `frames` (packed little-endian, the layout [`decode_raw`] reads).
pub fn encode_raw(frames: &[RawFrame]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        for s in &f.data {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::Cfa;

    fn spec() -> RawSpec {
        RawSpec {
            width: 4,
            height: 2,
            cfa: Cfa::Rggb,
            bits: 10,
            exposure_us: 1000.0,
            gain: 2.0,
        }
    }

    #[test]
    fn raw_files_round_trip_and_mask_the_bits() {
        let frames = vec![RawFrame {
            width: 4,
            height: 2,
            cfa: Cfa::Rggb,
            bits: 10,
            data: (0..8).map(|v| v * 100).collect(),
            exposure_us: 1000.0,
            analogue_gain: 2.0,
            digital_gain: 1.0,
            black_level: None,
        }];
        let back = decode_raw(&encode_raw(&frames), &spec()).unwrap();
        assert_eq!(back, frames);
        // Bits above the depth are dropped.
        let mut bytes = encode_raw(&frames);
        bytes[0] = 0xff;
        bytes[1] = 0xff;
        assert_eq!(decode_raw(&bytes, &spec()).unwrap()[0].data[0], 1023);
    }

    #[test]
    fn damaged_raw_files_are_refused() {
        let bytes = encode_raw(&[]);
        assert!(decode_raw(&bytes, &spec()).is_err());
        assert!(decode_raw(&[0; 15], &spec()).is_err());
        assert!(decode_raw(&[0; 16], &RawSpec { bits: 7, ..spec() }).is_err());
        assert!(decode_raw(&[0; 16], &RawSpec { width: 0, ..spec() }).is_err());
        assert_eq!(decode_raw(&[0; 16], &spec()).unwrap().len(), 1);
    }
}

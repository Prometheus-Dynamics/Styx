//! Raw recordings: frames as the sensor delivered them plus what produced each one, for
//! replaying the loop on a host.
//!
//! Two files per recording: `<base>.jsonl` (a header line `{"styx_raw_recording": 1, ...}`
//! with the raw format and the sensor mode, then one line per frame with its sensor values,
//! timestamp and where its bytes are) and `<base>.raw` (the frames back to back).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use styx_softisp::RawFormat;
#[cfg(test)]
use styx_softisp::RawPacking;

use crate::controller::SensorValues;
use crate::error::{PipelineError, Result};
use crate::sensor::SensorInfo;

/// Format version.
pub const VERSION: u32 = 1;

/// The first line of the index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// [`VERSION`].
    pub styx_raw_recording: u32,
    /// Raw layout of every frame.
    pub format: RawFormat,
    /// Bytes per row.
    pub stride: usize,
    /// The sensor mode.
    pub sensor: SensorInfo,
    /// Free-form notes (device, scene).
    #[serde(default)]
    pub notes: String,
}

/// One frame of the index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameRecord {
    /// What produced the frame.
    pub sensor: SensorValues,
    /// Capture timestamp (`CLOCK_MONOTONIC`), nanoseconds.
    pub timestamp_ns: u64,
    /// Offset of the frame in the `.raw` file.
    pub offset: u64,
    /// Bytes.
    pub len: u64,
}

fn paths(base: &Path) -> (PathBuf, PathBuf) {
    (base.with_extension("jsonl"), base.with_extension("raw"))
}

fn bad(e: impl std::fmt::Display) -> PipelineError {
    PipelineError::Recording(e.to_string())
}

/// Writes a recording.
pub struct RawWriter {
    index: BufWriter<File>,
    data: BufWriter<File>,
    offset: u64,
    frame_len: usize,
    frames: usize,
}

impl RawWriter {
    /// Creates `<base>.jsonl` and `<base>.raw`.
    pub fn create(base: &Path, header: &Header) -> Result<Self> {
        let (index, data) = paths(base);
        let mut index = BufWriter::new(File::create(index)?);
        writeln!(index, "{}", serde_json::to_string(header).map_err(bad)?)?;
        Ok(Self {
            index,
            data: BufWriter::new(File::create(data)?),
            offset: 0,
            frame_len: header.stride * header.format.height as usize,
            frames: 0,
        })
    }

    /// Appends a frame (its first `stride × height` bytes).
    pub fn write(&mut self, raw: &[u8], sensor: &SensorValues, timestamp_ns: u64) -> Result<()> {
        let bytes = raw.get(..self.frame_len).ok_or_else(|| {
            bad(format!(
                "frame of {} bytes, need {}",
                raw.len(),
                self.frame_len
            ))
        })?;
        self.data.write_all(bytes)?;
        let rec = FrameRecord {
            sensor: *sensor,
            timestamp_ns,
            offset: self.offset,
            len: bytes.len() as u64,
        };
        writeln!(self.index, "{}", serde_json::to_string(&rec).map_err(bad)?)?;
        self.offset += bytes.len() as u64;
        self.frames += 1;
        Ok(())
    }

    /// Frames written.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Flushes both files.
    pub fn finish(mut self) -> Result<()> {
        self.index.flush()?;
        self.data.flush()?;
        Ok(())
    }
}

/// A recording loaded into memory.
#[derive(Clone, Debug)]
pub struct RawRecording {
    /// The header.
    pub header: Header,
    /// The frames.
    pub frames: Vec<FrameRecord>,
    data: Vec<u8>,
}

impl RawRecording {
    /// Reads `<base>.jsonl` and `<base>.raw`.
    pub fn open(base: &Path) -> Result<Self> {
        let (index, data_path) = paths(base);
        let index = BufReader::new(File::open(index)?);
        let mut data = Vec::new();
        File::open(data_path)?.read_to_end(&mut data)?;
        Self::from_reader(index, data)
    }

    /// A recording from its index (the `.jsonl` lines) and its frames' bytes (checked).
    pub fn from_reader(index: impl BufRead, data: Vec<u8>) -> Result<Self> {
        let mut lines = index.lines();
        let header: Header =
            serde_json::from_str(&lines.next().ok_or_else(|| bad("empty index"))??).map_err(bad)?;
        if header.styx_raw_recording != VERSION {
            return Err(bad(format!("version {}", header.styx_raw_recording)));
        }
        let mut frames = Vec::new();
        for line in lines {
            let line = line?;
            if !line.trim().is_empty() {
                frames.push(serde_json::from_str(&line).map_err(bad)?);
            }
        }
        Self::from_parts(header, frames, data)
    }

    /// A recording from its parts (checked).
    pub fn from_parts(header: Header, frames: Vec<FrameRecord>, data: Vec<u8>) -> Result<Self> {
        let f = header.format;
        if f.width == 0 || f.height == 0 {
            return Err(bad("empty frames"));
        }
        if !(1..=16).contains(&f.packing.bit_depth()) {
            return Err(bad(format!("{} bits per sample", f.packing.bit_depth())));
        }
        if frames.is_empty() {
            return Err(bad("no frames"));
        }
        let row = f.min_stride();
        if header.stride < row {
            return Err(bad(format!("stride {} below {row}", header.stride)));
        }
        let need = header
            .stride
            .checked_mul(f.height as usize - 1)
            .and_then(|n| n.checked_add(row))
            .ok_or_else(|| bad("frames larger than memory"))?;
        for (i, f) in frames.iter().enumerate() {
            let end = f.offset.checked_add(f.len);
            if (f.len as usize) < need || end.is_none_or(|end| end > data.len() as u64) {
                return Err(bad(format!("frame {i} is outside the data or too short")));
            }
        }
        Ok(Self {
            header,
            frames,
            data,
        })
    }

    /// Frame `i`'s bytes.
    pub fn frame(&self, i: usize) -> &[u8] {
        let f = &self.frames[i];
        &self.data[f.offset as usize..(f.offset + f.len) as usize]
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// No frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

pub use crate::reexpose::unpack_row;

/// Packs 10-bit samples into a CSI-2 RAW10 row.
pub fn pack_raw10_row(samples: &[u16], out: &mut [u8]) {
    for (g, chunk) in samples.chunks(4).enumerate() {
        let o = &mut out[g * 5..g * 5 + 5];
        o[4] = 0;
        for (i, &v) in chunk.iter().enumerate() {
            o[i] = (v >> 2) as u8;
            o[4] |= ((v & 3) as u8) << (i * 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use styx_softisp::CfaPattern;

    use super::*;
    use crate::sensor::{SensorInfo, ov9782};

    #[test]
    fn raw10_round_trips() {
        let s: Vec<u16> = (0..8).map(|i| i * 131 % 1024).collect();
        let mut packed = vec![0u8; 10];
        pack_raw10_row(&s, &mut packed);
        let mut back = vec![0u16; 8];
        unpack_row(RawPacking::Csi2Raw10, &packed, 8, &mut back);
        assert_eq!(back, s);
    }

    #[test]
    fn recordings_round_trip() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("rawrec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rec");
        let info = SensorInfo::from_description(&ov9782(), "640x400", "raw10").unwrap();
        let format = RawFormat::new(8, 4, CfaPattern::Bggr, RawPacking::Csi2Raw10);
        let header = Header {
            styx_raw_recording: VERSION,
            format,
            stride: 16,
            sensor: info,
            notes: "test".into(),
        };
        let mut w = RawWriter::create(&base, &header).unwrap();
        let v = SensorValues {
            frame: 3,
            exposure: Duration::from_micros(5000),
            analogue_gain: 2.0,
            digital_gain: 1.0,
            frame_duration: Duration::from_micros(33333),
            verified: true,
        };
        w.write(&[7u8; 70], &v, 99).unwrap();
        assert!(w.write(&[0u8; 10], &v, 0).is_err());
        w.finish().unwrap();
        let r = RawRecording::open(&base).unwrap();
        assert_eq!(r.header, header);
        assert_eq!(r.len(), 1);
        assert_eq!(r.frames[0].sensor, v);
        assert_eq!(r.frame(0), &[7u8; 64][..]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Writes a small recording (index, a NUL byte, the frames) to
    /// `$STYX_FUZZ_SEEDS/pipeline_rawrec/` as a seed for that fuzz target (`docs/fuzzing.md`).
    #[test]
    #[ignore = "writes fuzz seeds; run with STYX_FUZZ_SEEDS set"]
    fn write_fuzz_seeds() {
        let Some(dir) = std::env::var_os("STYX_FUZZ_SEEDS") else {
            return;
        };
        let dir = Path::new(&dir).join("pipeline_rawrec");
        std::fs::create_dir_all(&dir).unwrap();
        let info = SensorInfo::from_description(&ov9782(), "640x400", "raw10").unwrap();
        let header = Header {
            styx_raw_recording: VERSION,
            format: RawFormat::new(8, 4, CfaPattern::Bggr, RawPacking::Csi2Raw10),
            stride: 10,
            sensor: info,
            notes: String::new(),
        };
        let mut bytes = serde_json::to_vec(&header).unwrap();
        for (i, gain) in [1.0, 2.0].into_iter().enumerate() {
            let rec = FrameRecord {
                sensor: SensorValues {
                    frame: i as u64,
                    exposure: Duration::from_micros(5000),
                    analogue_gain: gain,
                    digital_gain: 1.0,
                    frame_duration: Duration::from_micros(33333),
                    verified: true,
                },
                timestamp_ns: i as u64 * 33_333_000,
                offset: i as u64 * 40,
                len: 40,
            };
            bytes.push(b'\n');
            bytes.extend(serde_json::to_vec(&rec).unwrap());
        }
        bytes.push(0);
        bytes.extend((0..80u8).map(|i| i.wrapping_mul(37)));
        std::fs::write(dir.join("ov9782"), bytes).unwrap();
    }

    #[test]
    fn broken_indexes_are_rejected() {
        let info = SensorInfo::from_description(&ov9782(), "640x400", "raw10").unwrap();
        let header = |width, height, packing, stride| Header {
            styx_raw_recording: VERSION,
            format: RawFormat::new(width, height, CfaPattern::Bggr, packing),
            stride,
            sensor: info.clone(),
            notes: String::new(),
        };
        let frame = |offset, len| FrameRecord {
            sensor: SensorValues {
                frame: 0,
                exposure: Duration::from_micros(5000),
                analogue_gain: 1.0,
                digital_gain: 1.0,
                frame_duration: Duration::from_micros(33333),
                verified: true,
            },
            timestamp_ns: 0,
            offset,
            len,
        };
        let open = |h, frames| RawRecording::from_parts(h, frames, vec![0; 64]);
        let raw8 = RawPacking::U8;
        assert!(open(header(8, 4, raw8, 8), vec![frame(0, 32)]).is_ok());
        // Each of these panicked (underflow, overflow) or replayed garbage before.
        assert!(open(header(8, 0, raw8, 8), vec![frame(0, 32)]).is_err());
        assert!(open(header(8, 4, raw8, usize::MAX), vec![frame(0, 32)]).is_err());
        assert!(open(header(8, 4, raw8, 8), vec![frame(u64::MAX, 32)]).is_err());
        assert!(open(header(8, 4, raw8, 8), Vec::new()).is_err());
        let wide = RawPacking::U16Le { bits: 40 };
        assert!(open(header(4, 4, wide, 8), vec![frame(0, 32)]).is_err());
    }
}

//! Video files a replay plays besides recordings, without ffmpeg: Motion JPEG streams
//! (`.mjpeg`: JPEG images back to back), AVI files of Motion JPEG (`MJPG`), and directories of
//! JPEG images (in name order). Frames are delivered as `MJPG` (the planner decodes them as it
//! does a webcam's), timestamped by the AVI's frame interval or the given rate.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use styx_core::prelude::*;

use super::{RecordingFrames, RecordingHeader, ReplayError, frame_from_payload, open_recording};
use crate::DeviceIdentity;

/// The rate files without one of their own play at.
pub(crate) const DEFAULT_FPS: f32 = 30.0;

/// The largest JPEG read (a frame larger than this is taken for a damaged file).
const MAX_JPEG: usize = 64 << 20;

/// A recording or a video file at `path` (`fps`: the rate of files without one).
pub(crate) fn open_source(
    path: &Path,
    fps: Option<f32>,
) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
    let fps = fps.filter(|f| *f > 0.0).unwrap_or(DEFAULT_FPS);
    if path.is_dir() {
        return MediaFrames::open(Media::dir(path)?, fps, path);
    }
    let mut magic = [0u8; 12];
    let n = File::open(path)?.read(&mut magic)?;
    if n >= 3 && magic[..3] == [0xFF, 0xD8, 0xFF] {
        return MediaFrames::open(Media::Mjpeg(BufReader::new(File::open(path)?)), fps, path);
    }
    if n >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"AVI " {
        let mut r = BufReader::new(File::open(path)?);
        let interval = avi_header(&mut r)?;
        let fps = interval.map_or(fps, |us| 1e6 / us.max(1) as f32);
        return MediaFrames::open(Media::Avi(r), fps, path);
    }
    open_recording(path)
}

enum Media {
    Mjpeg(BufReader<File>),
    /// Positioned in the `movi` list.
    Avi(BufReader<File>),
    Dir(std::vec::IntoIter<PathBuf>),
}

impl Media {
    fn dir(path: &Path) -> Result<Self, ReplayError> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
            })
            .collect();
        files.sort();
        Ok(Media::Dir(files.into_iter()))
    }

    /// The next JPEG image (`None` at the end).
    fn next_jpeg(&mut self) -> Result<Option<Vec<u8>>, ReplayError> {
        match self {
            Media::Mjpeg(r) => read_jpeg(r),
            Media::Avi(r) => avi_frame(r),
            Media::Dir(files) => files
                .next()
                .map(std::fs::read)
                .transpose()
                .map_err(Into::into),
        }
    }
}

/// Frames of a video file.
pub(crate) struct MediaFrames {
    media: Media,
    format: MediaFormat,
    interval_ns: u64,
    index: u64,
    pub(crate) offset: u64,
    /// The first image, read to learn the format.
    first: Option<Vec<u8>>,
}

impl MediaFrames {
    fn open(
        mut media: Media,
        fps: f32,
        path: &Path,
    ) -> Result<(RecordingHeader, RecordingFrames), ReplayError> {
        let first = media
            .next_jpeg()?
            .ok_or(ReplayError::Corrupt("no JPEG image"))?;
        let (w, h) =
            jpeg_size(&first).ok_or(ReplayError::Corrupt("JPEG without a frame header"))?;
        let resolution = Resolution::new(w, h).ok_or(ReplayError::Corrupt("JPEG of no size"))?;
        let format = MediaFormat::new(FourCc::MJPG, resolution, ColorSpace::Srgb);
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        let header = RecordingHeader {
            device: DeviceIdentity {
                display: name,
                keys: vec![format!("file:{}", path.display())],
            },
            backend: "file".into(),
            format,
            interval: Interval::from_fps(fps.round().max(1.0) as u32),
        };
        let frames = MediaFrames {
            media,
            format,
            interval_ns: (1e9 / f64::from(fps)) as u64,
            index: 0,
            offset: 0,
            first: Some(first),
        };
        Ok((header, RecordingFrames::media(frames)))
    }
}

impl Iterator for MediaFrames {
    type Item = Result<FrameLease, ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        let jpeg = match self.first.take() {
            Some(j) => j,
            None => match self.media.next_jpeg() {
                Ok(Some(j)) => j,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            },
        };
        let timestamp = self.offset + self.index * self.interval_ns;
        self.index += 1;
        Some(frame_from_payload(self.format, timestamp, &jpeg, true))
    }
}

/// Reads exactly `n` bytes; `None` at the end of the input.
fn read_n(r: &mut impl Read, n: usize) -> Result<Option<Vec<u8>>, ReplayError> {
    let mut buf = Vec::new();
    let got = r.take(n as u64).read_to_end(&mut buf)?;
    Ok((got == n).then_some(buf))
}

/// The next JPEG of a Motion JPEG stream: its segments up to the scan, then the entropy-coded
/// data up to the marker after it (so thumbnails in its metadata do not end it early).
fn read_jpeg(r: &mut impl Read) -> Result<Option<Vec<u8>>, ReplayError> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    // Find the start of image (skipping anything between images).
    let mut prev = 0u8;
    loop {
        if r.read(&mut byte)? == 0 {
            return Ok(None);
        }
        if prev == 0xFF && byte[0] == 0xD8 {
            break;
        }
        prev = byte[0];
    }
    out.extend_from_slice(&[0xFF, 0xD8]);
    // Segments: FF xx, then a big-endian length including itself.
    loop {
        let Some(m) = read_n(r, 2)? else {
            return Err(ReplayError::Corrupt("JPEG cut short"));
        };
        if m[0] != 0xFF {
            return Err(ReplayError::Corrupt("JPEG segment without a marker"));
        }
        out.extend_from_slice(&m);
        if m[1] == 0xD9 {
            return Ok(Some(out));
        }
        let Some(len) = read_n(r, 2)? else {
            return Err(ReplayError::Corrupt("JPEG cut short"));
        };
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        out.extend_from_slice(&(len as u16).to_be_bytes());
        let Some(body) = read_n(r, len.saturating_sub(2))? else {
            return Err(ReplayError::Corrupt("JPEG cut short"));
        };
        out.extend_from_slice(&body);
        if m[1] == 0xDA {
            break;
        }
    }
    // Entropy-coded data: a marker is FF followed by neither 00 (a stuffed byte) nor a restart.
    loop {
        if r.read(&mut byte)? == 0 {
            return Err(ReplayError::Corrupt("JPEG cut short"));
        }
        out.push(byte[0]);
        if out.len() > MAX_JPEG {
            return Err(ReplayError::Corrupt("JPEG too large"));
        }
        // The end of image: FF D9 (a stuffed FF 00 or a restart is not a marker; another
        // scan's markers, in progressive streams, are kept as data).
        let n = out.len();
        if n >= 2 && out[n - 2] == 0xFF && out[n - 1] == 0xD9 {
            return Ok(Some(out));
        }
    }
}

/// The width and height in a JPEG's frame header (SOF0 to SOF15, not DHT/JPG/DAC).
fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let s = jpeg.get(i + 5..i + 9)?;
            let h = u32::from(u16::from_be_bytes([s[0], s[1]]));
            let w = u32::from(u16::from_be_bytes([s[2], s[3]]));
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Reads an AVI's header lists up to its `movi` list (the reader is left at its first chunk);
/// returns the frame interval in microseconds (`avih`), if any.
fn avi_header(r: &mut impl Read) -> Result<Option<u32>, ReplayError> {
    let corrupt = || ReplayError::Corrupt("AVI without a movi list");
    read_n(r, 12)?.ok_or_else(corrupt)?; // RIFF size AVI
    let mut interval = None;
    loop {
        let head = read_n(r, 8)?.ok_or_else(corrupt)?;
        let size = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes")) as usize;
        if &head[..4] == b"LIST" {
            let kind = read_n(r, 4)?.ok_or_else(corrupt)?;
            if &kind[..] == b"movi" {
                return Ok(interval);
            }
            if &kind[..] == b"hdrl" {
                continue; // its chunks follow: avih among them
            }
            skip(r, size.saturating_sub(4) + size % 2)?;
        } else if &head[..4] == b"avih" {
            let body = read_n(r, size + size % 2)?.ok_or_else(corrupt)?;
            interval = body
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                .filter(|&us| us > 0);
        } else {
            skip(r, size + size % 2)?;
        }
    }
}

/// The next video frame chunk (`##dc` / `##db`) of an AVI's `movi` list; `None` at its end.
fn avi_frame(r: &mut impl Read) -> Result<Option<Vec<u8>>, ReplayError> {
    loop {
        let Some(head) = read_n(r, 8)? else {
            return Ok(None);
        };
        let size = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes")) as usize;
        match &head[..4] {
            b"LIST" => {
                // `rec ` lists hold chunks: read on into them.
                read_n(r, 4)?;
            }
            b"idx1" => return Ok(None),
            [_, _, b'd', b'c'] | [_, _, b'd', b'b'] => {
                if size > MAX_JPEG {
                    return Err(ReplayError::Corrupt("AVI frame too large"));
                }
                let data = read_n(r, size)?.ok_or(ReplayError::Corrupt("AVI cut short"))?;
                skip(r, size % 2)?;
                if size > 0 {
                    return Ok(Some(data));
                }
            }
            _ => skip(r, size + size % 2)?,
        }
    }
}

fn skip(r: &mut impl Read, n: usize) -> Result<(), ReplayError> {
    std::io::copy(&mut r.take(n as u64), &mut std::io::sink())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../../../testing/fixtures/c270_720p_rst.mjpeg");

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("styx-media-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn jpegs() -> Vec<Vec<u8>> {
        let mut r = std::io::Cursor::new(FIXTURE);
        std::iter::from_fn(|| read_jpeg(&mut r).unwrap()).collect()
    }

    fn timestamps(path: &Path, fps: Option<f32>) -> (RecordingHeader, Vec<u64>) {
        let (header, frames) = open_source(path, fps).unwrap();
        let ts = frames.map(|f| f.unwrap().meta().timestamp).collect();
        (header, ts)
    }

    #[test]
    fn mjpeg_streams_avi_files_and_jpeg_directories_replay_as_mjpg_frames() {
        let images = jpegs();
        assert!(images.len() >= 3);
        let dir = scratch("all");
        // The stream as it is.
        let mjpeg = dir.join("cam.mjpeg");
        std::fs::write(&mjpeg, FIXTURE).unwrap();
        let (header, ts) = timestamps(&mjpeg, Some(10.0));
        assert_eq!(header.format.code, FourCc::MJPG);
        assert_eq!(
            header.format.resolution,
            Resolution::new(1280, 720).unwrap()
        );
        assert_eq!(ts.len(), images.len());
        assert_eq!(ts[1], 100_000_000);
        // An AVI at 25 fps (avih: 40000 us).
        let mut movi = Vec::new();
        for j in &images {
            movi.extend_from_slice(b"00dc");
            movi.extend_from_slice(&(j.len() as u32).to_le_bytes());
            movi.extend_from_slice(j);
            if j.len() % 2 == 1 {
                movi.push(0);
            }
        }
        let mut avih = vec![0u8; 56];
        avih[..4].copy_from_slice(&40_000u32.to_le_bytes());
        let mut hdrl = b"hdrl".to_vec();
        hdrl.extend_from_slice(b"avih");
        hdrl.extend_from_slice(&56u32.to_le_bytes());
        hdrl.extend_from_slice(&avih);
        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
        body.extend_from_slice(&hdrl);
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&((movi.len() + 4) as u32).to_le_bytes());
        body.extend_from_slice(b"movi");
        body.extend_from_slice(&movi);
        let mut avi = b"RIFF".to_vec();
        avi.extend_from_slice(&(body.len() as u32).to_le_bytes());
        avi.extend_from_slice(&body);
        let avi_path = dir.join("cam.avi");
        std::fs::write(&avi_path, &avi).unwrap();
        let (header, ts) = timestamps(&avi_path, None);
        assert_eq!(header.interval, Interval::from_fps(25));
        assert_eq!(ts.len(), images.len());
        assert_eq!(ts[2], 80_000_000);
        // A directory of JPEGs, in name order.
        let seq = dir.join("seq");
        std::fs::create_dir_all(&seq).unwrap();
        for (i, j) in images.iter().enumerate() {
            std::fs::write(seq.join(format!("f{i:03}.jpg")), j).unwrap();
        }
        std::fs::write(seq.join("notes.txt"), b"x").unwrap();
        let (_, frames) = open_source(&seq, None).unwrap();
        let got: Vec<Vec<u8>> = frames
            .map(|f| f.unwrap().planes()[0].data().to_vec())
            .collect();
        assert_eq!(got.len(), images.len());
        assert_eq!(&got[1][..], &images[1][..]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! Frame arithmetic: CSI-2 packed raw10, mean levels, grey PGM output, frame rate and
//! sequence statistics, and finding where a level change lands.

use std::io::{self, Write};
use std::time::Duration;

use styx_kernel::FourCc;

/// Packed 10-bit Bayer and mono formats (`V4L2_PIX_FMT_S*10P`, `Y10P`): four pixels in five
/// bytes, the high 8 bits of each pixel first, then one byte of the four low 2-bit pairs.
pub const PACKED_RAW10: [FourCc; 5] = [
    FourCc::new(b"pBAA"),
    FourCc::new(b"pGAA"),
    FourCc::new(b"pgAA"),
    FourCc::new(b"pRAA"),
    FourCc::new(b"Y10P"),
];

/// Picks the packed raw10 format from what a node offers for a bus code, else the first.
pub fn choose_format(offered: &[FourCc]) -> Option<FourCc> {
    offered
        .iter()
        .find(|f| PACKED_RAW10.contains(f))
        .or_else(|| offered.first())
        .copied()
}

/// Unpacks one line of packed raw10 into `out` (10-bit values). `line` must hold at least
/// `width * 5 / 4` bytes; `width` must be a multiple of 4.
pub fn unpack_raw10_line(line: &[u8], width: usize, out: &mut Vec<u16>) {
    out.clear();
    for group in line[..width / 4 * 5].as_chunks::<5>().0 {
        let low = group[4];
        for (i, &hi) in group[..4].iter().enumerate() {
            out.push((u16::from(hi) << 2) | u16::from((low >> (2 * i)) & 3));
        }
    }
}

/// Geometry of a packed raw10 frame in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raw10Layout {
    /// Pixels per line.
    pub width: usize,
    /// Lines.
    pub height: usize,
    /// Bytes per line (at least `width * 5 / 4`).
    pub stride: usize,
}

impl Raw10Layout {
    /// Bytes a frame needs.
    pub fn frame_bytes(&self) -> usize {
        self.stride * self.height
    }

    fn check(&self, data: &[u8]) -> io::Result<()> {
        if !self.width.is_multiple_of(4)
            || self.stride < self.width / 4 * 5
            || data.len() < self.frame_bytes()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} bytes do not hold a {}x{} raw10 frame with stride {}",
                    data.len(),
                    self.width,
                    self.height,
                    self.stride
                ),
            ));
        }
        Ok(())
    }

    /// Mean 10-bit level over every `row_step`-th line (1 = all lines). Only the high 8 bits
    /// of each pixel are read (×4 + 1.5, the mean of the dropped bits), so a line costs
    /// `width` byte reads: frame buffers may be uncached.
    pub fn mean_level(&self, data: &[u8], row_step: usize) -> io::Result<f64> {
        self.check(data)?;
        let mut sum = 0u64;
        let mut n = 0u64;
        for row in (0..self.height).step_by(row_step.max(1)) {
            let line = &data[row * self.stride..][..self.width / 4 * 5];
            for group in line.as_chunks::<5>().0 {
                sum += group[..4].iter().map(|&b| u64::from(b)).sum::<u64>();
                n += 4;
            }
        }
        Ok(if n == 0 {
            0.0
        } else {
            sum as f64 * 4.0 / n as f64 + 1.5
        })
    }

    /// Grey image from a Bayer frame: each 2x2 cell averaged, scaled to 8 bits
    /// (`width / 2` x `height / 2`).
    pub fn grey_half(&self, data: &[u8]) -> io::Result<(usize, usize, Vec<u8>)> {
        self.check(data)?;
        let (w, h) = (self.width / 2, self.height / 2);
        let mut out = Vec::with_capacity(w * h);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for y in 0..h {
            unpack_raw10_line(&data[2 * y * self.stride..], self.width, &mut a);
            unpack_raw10_line(&data[(2 * y + 1) * self.stride..], self.width, &mut b);
            for x in 0..w {
                let s = u32::from(a[2 * x]) + u32::from(a[2 * x + 1]);
                let s = s + u32::from(b[2 * x]) + u32::from(b[2 * x + 1]);
                // Sum of four 10-bit values -> 8 bits: / 4 / 4.
                out.push((s / 16).min(255) as u8);
            }
        }
        Ok((w, h, out))
    }
}

/// Writes an 8-bit binary PGM.
pub fn write_pgm(
    out: &mut impl Write,
    width: usize,
    height: usize,
    pixels: &[u8],
) -> io::Result<()> {
    write!(out, "P5\n{width} {height}\n255\n")?;
    out.write_all(&pixels[..width * height])
}

/// One dequeued frame's summary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSample {
    /// Driver sequence number.
    pub sequence: u32,
    /// Capture timestamp.
    pub timestamp: Duration,
    /// Mean 10-bit level.
    pub mean: f64,
    /// The driver flagged the buffer as corrupted.
    pub error: bool,
}

/// Statistics over a run of frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunStats {
    /// Frames seen.
    pub frames: usize,
    /// Frames missing according to the sequence numbers.
    pub missing: u32,
    /// Largest single sequence gap (0 = none).
    pub largest_gap: u32,
    /// Frames flagged with errors.
    pub errors: usize,
    /// Frame rate from the timestamps of the first and last frame.
    pub fps: f64,
    /// Mean level over all frames.
    pub mean: f64,
}

impl RunStats {
    /// Statistics over `frames` (in dequeue order).
    pub fn of(frames: &[FrameSample]) -> Self {
        let mut missing = 0;
        let mut largest_gap = 0;
        for w in frames.windows(2) {
            let gap = w[1].sequence.wrapping_sub(w[0].sequence).saturating_sub(1);
            missing += gap;
            largest_gap = largest_gap.max(gap);
        }
        let fps = match (frames.first(), frames.last()) {
            (Some(a), Some(b)) if b.timestamp > a.timestamp => {
                f64::from(b.sequence.wrapping_sub(a.sequence))
                    / (b.timestamp - a.timestamp).as_secs_f64()
            }
            _ => 0.0,
        };
        let mean = if frames.is_empty() {
            0.0
        } else {
            frames.iter().map(|f| f.mean).sum::<f64>() / frames.len() as f64
        };
        Self {
            frames: frames.len(),
            missing,
            largest_gap,
            errors: frames.iter().filter(|f| f.error).count(),
            fps,
            mean,
        }
    }
}

/// The first frame at or after `from` whose level has moved at least halfway from `before`
/// towards `expected` (and stays there for the next frame when there is one).
pub fn level_change_frame(
    frames: &[FrameSample],
    from: u32,
    before: f64,
    expected: f64,
) -> Option<u32> {
    let half = (before + expected) / 2.0;
    let moved = |m: f64| {
        if expected >= before {
            m >= half
        } else {
            m <= half
        }
    };
    let after: Vec<&FrameSample> = frames
        .iter()
        .filter(|f| f.sequence.wrapping_sub(from) < u32::MAX / 2)
        .collect();
    after.iter().enumerate().find_map(|(i, f)| {
        let next_ok = after.get(i + 1).is_none_or(|n| moved(n.mean));
        (moved(f.mean) && next_ok).then_some(f.sequence)
    })
}

/// The level expected after scaling exposure by `ratio`: the part above black scales,
/// clamped to the 10-bit range.
pub fn expected_level(before: f64, black: f64, ratio: f64) -> f64 {
    (black + (before - black).max(0.0) * ratio).clamp(0.0, 1023.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(pixels: &[u16]) -> Vec<u8> {
        pixels
            .chunks(4)
            .flat_map(|p| {
                let low = p
                    .iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, v)| acc | (((v & 3) as u8) << (2 * i)));
                p.iter().map(|v| (v >> 2) as u8).chain([low])
            })
            .collect()
    }

    #[test]
    fn unpacks_raw10() {
        let px = [0u16, 1, 2, 3, 1023, 512, 64, 700];
        let mut out = Vec::new();
        unpack_raw10_line(&pack(&px), 8, &mut out);
        assert_eq!(out, px);
    }

    fn frame(
        width: usize,
        height: usize,
        stride: usize,
        f: impl Fn(usize, usize) -> u16,
    ) -> Vec<u8> {
        let mut data = vec![0xeeu8; stride * height];
        for y in 0..height {
            let row: Vec<u16> = (0..width).map(|x| f(x, y)).collect();
            let packed = pack(&row);
            data[y * stride..y * stride + packed.len()].copy_from_slice(&packed);
        }
        data
    }

    #[test]
    fn means_and_grey_images() {
        let l = Raw10Layout {
            width: 8,
            height: 4,
            stride: 16,
        };
        let data = frame(
            8,
            4,
            16,
            |x, y| if y % 2 == 0 { 400 + x as u16 * 4 } else { 200 },
        );
        let m = l.mean_level(&data, 1).unwrap();
        assert!((m - (414.0 + 200.0) / 2.0).abs() < 2.0, "{m}");
        let top = l.mean_level(&data, 2).unwrap();
        assert!((top - 415.5).abs() < 2.0, "{top}");
        let (w, h, g) = l.grey_half(&data).unwrap();
        assert_eq!((w, h), (4, 2));
        // (400 + 404 + 200 + 200) / 16 = 75
        assert_eq!(g[0], 75);
        let mut pgm = Vec::new();
        write_pgm(&mut pgm, w, h, &g).unwrap();
        assert!(pgm.starts_with(b"P5\n4 2\n255\n"));
        assert_eq!(pgm.len(), 11 + 8);
        assert!(l.mean_level(&data[..40], 1).is_err());
    }

    #[test]
    fn prefers_packed_formats() {
        let bg16 = FourCc::new(b"BG16");
        let pbaa = FourCc::new(b"pBAA");
        assert_eq!(choose_format(&[bg16, pbaa]), Some(pbaa));
        assert_eq!(choose_format(&[bg16]), Some(bg16));
        assert_eq!(choose_format(&[]), None);
    }

    fn samples(seqs: &[u32], period_ms: u64, means: &[f64]) -> Vec<FrameSample> {
        seqs.iter()
            .zip(means)
            .map(|(&s, &m)| FrameSample {
                sequence: s,
                timestamp: Duration::from_millis(1000 + u64::from(s) * period_ms),
                mean: m,
                error: false,
            })
            .collect()
    }

    #[test]
    fn run_statistics() {
        let f = samples(&[10, 11, 12, 15, 16], 10, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        let s = RunStats::of(&f);
        assert_eq!((s.frames, s.missing, s.largest_gap, s.errors), (5, 2, 2, 0));
        assert!((s.fps - 100.0).abs() < 1e-9);
        assert!((s.mean - 3.0).abs() < 1e-9);
        assert_eq!(RunStats::of(&[]).fps, 0.0);
    }

    #[test]
    fn finds_level_changes() {
        let f = samples(
            &[0, 1, 2, 3, 4, 5, 6],
            33,
            &[100.0, 100.0, 101.0, 180.0, 99.0, 190.0, 191.0],
        );
        // A one-frame spike at 3 is ignored; the change holds from 5.
        assert_eq!(level_change_frame(&f, 0, 100.0, 196.0), Some(5));
        assert_eq!(level_change_frame(&f, 6, 100.0, 196.0), Some(6));
        let down = samples(&[0, 1, 2], 33, &[190.0, 120.0, 100.0]);
        assert_eq!(level_change_frame(&down, 0, 190.0, 100.0), Some(1));
        assert_eq!(level_change_frame(&down, 0, 190.0, 0.0), None);
        assert!((expected_level(164.0, 64.0, 2.0) - 264.0).abs() < 1e-9);
        assert_eq!(expected_level(900.0, 64.0, 4.0), 1023.0);
    }
}

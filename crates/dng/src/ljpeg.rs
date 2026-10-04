//! Lossless JPEG (ITU T.81 process 14, `SOF3`) decoding, as DNG files from cameras and from
//! Adobe's converter compress their raw tiles (`Compression` = 7).

use alloc::{format, string::String, vec, vec::Vec};

use crate::{DngError, Result};

fn err(msg: impl Into<String>) -> DngError {
    DngError::Malformed(format!("lossless JPEG: {}", msg.into()))
}

/// A decoded lossless JPEG: `width` x `height` pixels of `components` interleaved samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Pixels per line (as the frame header says, before components).
    pub width: usize,
    /// Lines.
    pub height: usize,
    /// Interleaved components per pixel.
    pub components: usize,
    /// Sample precision in bits.
    pub precision: u8,
    /// Samples, `height * width * components`, row-major, components interleaved.
    pub samples: Vec<u16>,
}

/// A Huffman table (T.81 F.2.2.3: `maxcode`, `valptr`, `mincode` by code length).
#[derive(Clone, Debug, Default)]
struct Huffman {
    mincode: [i32; 17],
    maxcode: [i32; 18],
    valptr: [i32; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: Vec<u8>) -> Result<Self> {
        let mut h = Huffman {
            maxcode: [-1; 18],
            values,
            ..Default::default()
        };
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if total > h.values.len() || total > 256 {
            return Err(err("Huffman table counts exceed its values"));
        }
        let (mut code, mut k) = (0i32, 0i32);
        for len in 1..=16 {
            let n = i32::from(counts[len - 1]);
            if n > 0 {
                h.valptr[len] = k;
                h.mincode[len] = code;
                code += n;
                k += n;
                h.maxcode[len] = code - 1;
            }
            code <<= 1;
        }
        h.maxcode[17] = i32::MAX;
        Ok(h)
    }
}

/// Reads entropy-coded bits, removing stuffed zero bytes; stops at a marker.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
    marker: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            acc: 0,
            n: 0,
            marker: false,
        }
    }

    fn fill(&mut self) {
        while self.n <= 56 {
            let mut byte = 0u8;
            if !self.marker && self.pos < self.data.len() {
                byte = self.data[self.pos];
                if byte == 0xff {
                    match self.data.get(self.pos + 1) {
                        Some(0) => self.pos += 2,
                        _ => {
                            // A marker: no more data in this segment.
                            self.marker = true;
                            byte = 0;
                        }
                    }
                } else {
                    self.pos += 1;
                }
            }
            self.acc |= u64::from(byte) << (56 - self.n);
            self.n += 8;
        }
    }

    fn bits(&mut self, count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        if self.n < count {
            self.fill();
        }
        let v = (self.acc >> (64 - count)) as u32;
        self.acc <<= count;
        self.n -= count;
        v
    }

    fn decode(&mut self, h: &Huffman) -> Result<u8> {
        let mut code = 0i32;
        for len in 1..=16 {
            code = (code << 1) | self.bits(1) as i32;
            if code <= h.maxcode[len] {
                let i = h.valptr[len] + code - h.mincode[len];
                return h
                    .values
                    .get(i as usize)
                    .copied()
                    .ok_or_else(|| err("Huffman value out of range"));
            }
        }
        Err(err("bad Huffman code"))
    }

    /// Skips to just after the next restart marker.
    fn restart(&mut self) {
        let mut p = self.pos;
        while p + 1 < self.data.len() {
            if self.data[p] == 0xff && (0xd0..=0xd7).contains(&self.data[p + 1]) {
                p += 2;
                break;
            }
            p += 1;
        }
        let data = self.data;
        *self = Bits::new(&data[p.min(data.len())..]);
    }
}

fn be16(b: &[u8], at: usize) -> Result<usize> {
    b.get(at..at + 2)
        .map(|c| usize::from(u16::from_be_bytes([c[0], c[1]])))
        .ok_or_else(|| err("truncated"))
}

/// Decodes a lossless JPEG stream.
pub fn decode(data: &[u8]) -> Result<Decoded> {
    if data.get(..2) != Some(&[0xff, 0xd8]) {
        return Err(err("no SOI marker"));
    }
    let mut tables: [Option<Huffman>; 4] = Default::default();
    let mut frame: Option<(u8, usize, usize, Vec<u8>)> = None;
    let mut restart_interval = 0usize;
    let mut at = 2;
    loop {
        // Markers may be padded with 0xff.
        while data.get(at) == Some(&0xff) && data.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        let marker = *data.get(at + 1).ok_or_else(|| err("no SOS"))?;
        if data[at] != 0xff {
            return Err(err("expected a marker"));
        }
        let len = be16(data, at + 2)?;
        let seg = data
            .get(at + 4..at + 2 + len)
            .ok_or_else(|| err("segment truncated"))?;
        match marker {
            0xc3 => {
                if seg.len() < 6 {
                    return Err(err("SOF3 too short"));
                }
                let n = usize::from(seg[5]);
                let ids = (0..n)
                    .map(|i| seg.get(6 + 3 * i).copied())
                    .collect::<Option<Vec<u8>>>()
                    .ok_or_else(|| err("SOF3 components truncated"))?;
                frame = Some((seg[0], be16(seg, 1)?, be16(seg, 3)?, ids));
            }
            0xc0..=0xc2 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                return Err(DngError::Unsupported(format!(
                    "JPEG process SOF{:x} (only lossless SOF3)",
                    marker - 0xc0
                )));
            }
            0xc4 => {
                let mut p = 0;
                while p + 17 <= seg.len() {
                    let class_id = seg[p];
                    let mut counts = [0u8; 16];
                    counts.copy_from_slice(&seg[p + 1..p + 17]);
                    let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
                    let values = seg
                        .get(p + 17..p + 17 + n)
                        .ok_or_else(|| err("DHT truncated"))?
                        .to_vec();
                    tables[usize::from(class_id & 3)] = Some(Huffman::new(&counts, values)?);
                    p += 17 + n;
                }
            }
            0xdd => restart_interval = be16(seg, 0)?,
            0xda => {
                let (precision, height, width, ids) =
                    frame.take().ok_or_else(|| err("SOS before SOF3"))?;
                let ns = usize::from(*seg.first().ok_or_else(|| err("SOS empty"))?);
                if ns != ids.len() || ns == 0 || ns > 4 {
                    return Err(DngError::Unsupported(
                        "lossless JPEG with several scans".into(),
                    ));
                }
                let mut huff = Vec::with_capacity(ns);
                for i in 0..ns {
                    let sel = seg.get(2 + 2 * i).ok_or_else(|| err("SOS truncated"))? >> 4;
                    huff.push(
                        tables[usize::from(sel & 3)]
                            .clone()
                            .ok_or_else(|| err("missing Huffman table"))?,
                    );
                }
                let predictor = *seg.get(1 + 2 * ns).ok_or_else(|| err("SOS truncated"))?;
                let transform = seg.get(3 + 2 * ns).ok_or_else(|| err("SOS truncated"))? & 15;
                let scan = &data[at + 2 + len..];
                let samples = decode_scan(
                    scan,
                    (width, height, ns),
                    precision,
                    predictor,
                    transform,
                    &huff,
                    restart_interval,
                )?;
                return Ok(Decoded {
                    width,
                    height,
                    components: ns,
                    precision,
                    samples,
                });
            }
            0xd9 => return Err(err("EOI before a scan")),
            _ => {}
        }
        at += 2 + len;
    }
}

fn decode_scan(
    scan: &[u8],
    (width, height, ns): (usize, usize, usize),
    precision: u8,
    predictor: u8,
    transform: u8,
    huff: &[Huffman],
    restart_interval: usize,
) -> Result<Vec<u16>> {
    if !(2..=16).contains(&precision) || !(1..=7).contains(&predictor) || width == 0 {
        return Err(err(format!(
            "precision {precision}, predictor {predictor}, width {width}"
        )));
    }
    let row_len = width * ns;
    let total = row_len
        .checked_mul(height)
        .filter(|&t| t <= 1 << 30)
        .ok_or_else(|| err("image too large"))?;
    let mut out = vec![0u16; total];
    let mut bits = Bits::new(scan);
    let mask = (1u32 << precision) - 1;
    let initial = 1i32 << (precision - transform.min(precision - 1) - 1);
    let mut first_line = true;
    let mut since_restart = 0usize;
    for y in 0..height {
        let (done, rest) = out.split_at_mut(y * row_len);
        let prev = done
            .get(done.len().saturating_sub(row_len)..)
            .unwrap_or(&[]);
        let row = &mut rest[..row_len];
        for x in 0..width {
            if restart_interval > 0 && since_restart == restart_interval {
                bits.restart();
                since_restart = 0;
                first_line = true;
            }
            for (c, table) in huff.iter().enumerate() {
                let ssss = bits.decode(table)?;
                let diff: i32 = match ssss {
                    0 => 0,
                    16 => 32768,
                    s if s < 16 => {
                        let v = bits.bits(u32::from(s)) as i32;
                        if v < 1 << (s - 1) {
                            v - (1 << s) + 1
                        } else {
                            v
                        }
                    }
                    _ => return Err(err("difference category above 16")),
                };
                let i = x * ns + c;
                let ra = || i32::from(row[i - ns]);
                let pred = if first_line && x == 0 {
                    initial
                } else if first_line {
                    ra()
                } else if x == 0 {
                    i32::from(prev[i])
                } else {
                    let (a, b, cc) = (ra(), i32::from(prev[i]), i32::from(prev[i - ns]));
                    match predictor {
                        1 => a,
                        2 => b,
                        3 => cc,
                        4 => a + b - cc,
                        5 => a + ((b - cc) >> 1),
                        6 => b + ((a - cc) >> 1),
                        _ => (a + b) >> 1,
                    }
                };
                row[i] = ((pred + diff) as u32 & mask) as u16;
            }
            since_restart += 1;
        }
        first_line = false;
    }
    if transform > 0 {
        for v in &mut out {
            *v <<= transform;
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal lossless JPEG encoder (one table of 5-bit codes for categories 0..=16) for
    /// tests.
    pub(crate) fn encode(
        samples: &[u16],
        width: usize,
        height: usize,
        ns: usize,
        precision: u8,
        predictor: u8,
    ) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        let mut seg = |marker: u8, body: &[u8]| {
            out.extend_from_slice(&[0xff, marker]);
            out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(body);
        };
        let mut sof = vec![precision];
        sof.extend_from_slice(&(height as u16).to_be_bytes());
        sof.extend_from_slice(&(width as u16).to_be_bytes());
        sof.push(ns as u8);
        for c in 0..ns {
            sof.extend_from_slice(&[c as u8, 0x11, 0]);
        }
        seg(0xc3, &sof);
        let mut dht = vec![0x00];
        let mut counts = [0u8; 16];
        counts[4] = 17;
        dht.extend_from_slice(&counts);
        dht.extend(0..=16u8);
        seg(0xc4, &dht);
        let mut sos = vec![ns as u8];
        for c in 0..ns {
            sos.extend_from_slice(&[c as u8, 0x00]);
        }
        sos.extend_from_slice(&[predictor, 0, 0]);
        seg(0xda, &sos);
        // Entropy-coded data.
        let (mut acc, mut n) = (0u64, 0u32);
        let mut data = Vec::new();
        let mut put = |v: u32, len: u32, data: &mut Vec<u8>| {
            acc = (acc << len) | u64::from(v & ((1u32 << len) - 1));
            n += len;
            while n >= 8 {
                let b = (acc >> (n - 8)) as u8;
                data.push(b);
                if b == 0xff {
                    data.push(0);
                }
                n -= 8;
            }
        };
        let row_len = width * ns;
        let initial = 1i32 << (precision - 1);
        for y in 0..height {
            for x in 0..width {
                for c in 0..ns {
                    let i = y * row_len + x * ns + c;
                    let s = |j: usize| i32::from(samples[j]);
                    let pred = if y == 0 && x == 0 {
                        initial
                    } else if y == 0 {
                        s(i - ns)
                    } else if x == 0 {
                        s(i - row_len)
                    } else {
                        let (a, b, cc) = (s(i - ns), s(i - row_len), s(i - row_len - ns));
                        match predictor {
                            1 => a,
                            2 => b,
                            3 => cc,
                            4 => a + b - cc,
                            5 => a + ((b - cc) >> 1),
                            6 => b + ((a - cc) >> 1),
                            _ => (a + b) >> 1,
                        }
                    };
                    let mut diff = (s(i) - pred) & 0xffff;
                    if diff >= 32768 {
                        diff -= 65536;
                    }
                    let ssss = if diff == 0 {
                        0
                    } else {
                        32 - diff.unsigned_abs().leading_zeros()
                    };
                    put(ssss, 5, &mut data);
                    if ssss > 0 && ssss < 16 {
                        let v = if diff < 0 { diff - 1 } else { diff };
                        put(v as u32, ssss, &mut data);
                    }
                }
            }
        }
        // Pad the last byte with ones.
        put(0x7f, 7, &mut data);
        out.extend_from_slice(&data);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    #[test]
    fn decodes_every_predictor_and_component_count() {
        let (w, h) = (13, 7);
        for ns in [1usize, 2, 4] {
            let samples: Vec<u16> = (0..w * h * ns)
                .map(|i| ((i * 7919 + i * i * 31) % 4096) as u16)
                .collect();
            for predictor in 1..=7 {
                let jpeg = encode(&samples, w, h, ns, 12, predictor);
                let d = decode(&jpeg).unwrap();
                assert_eq!((d.width, d.height, d.components), (w, h, ns));
                assert_eq!(d.samples, samples, "ns {ns} predictor {predictor}");
            }
        }
        // 16-bit extremes (category 16 differences).
        let samples = vec![0u16, 65535, 0, 65535, 32768, 1];
        let d = decode(&encode(&samples, 3, 2, 1, 16, 1)).unwrap();
        assert_eq!(d.samples, samples);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&[0, 1, 2]).is_err());
        assert!(decode(&[0xff, 0xd8, 0xff, 0xd9, 0, 2]).is_err());
        let mut j = encode(&[1, 2, 3, 4], 2, 2, 1, 12, 1);
        j.truncate(12);
        assert!(decode(&j).is_err());
        // Baseline JPEG is not lossless.
        let mut b = encode(&[1, 2, 3, 4], 2, 2, 1, 12, 1);
        b[3] = 0xc0;
        assert!(decode(&b).is_err());
    }
}

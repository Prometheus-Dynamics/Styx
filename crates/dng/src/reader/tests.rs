use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::color::{self, Illuminant};
use crate::opcode::bayer_gain_maps;
use crate::raw::{Packing, RawImage, SampleLayout};
use crate::writer::{ColorCalibration, DngMetadata, Preview, write_dng};

const CCM: Matrix3 = [
    1.80439, -0.73699, -0.06739, -0.36073, 1.83327, -0.47255, -0.08378, -0.56403, 1.64781,
];

fn bayer(w: u32, h: u32) -> RawImage {
    let samples = (0..w * h).map(|i| ((i * 37) % 1024) as u16).collect();
    RawImage::new(w, h, SampleLayout::Cfa(CfaPattern::Bggr), 10, samples).unwrap()
}

fn close(a: &[f64], b: &[f64], tol: f64) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < tol)
}

#[test]
fn what_the_writer_writes_reads_back() {
    let img = bayer(64, 48);
    let neutral_a = [0.9, 1.0, 0.42];
    let neutral_d65 = [0.5, 1.0, 0.75];
    let cm_a = color::color_matrix(&CCM, neutral_a, Illuminant::StandardA.xy()).unwrap();
    let cm_d = color::color_matrix(&CCM, neutral_d65, Illuminant::D65.xy()).unwrap();
    let ones = vec![1.0; 12];
    let r: Vec<f64> = (0..12).map(|i| 1.0 + f64::from(i) / 10.0).collect();
    let meta = DngMetadata {
        black_level: [64.0, 64.0, 64.0, 65.5],
        white_level: Some(1023),
        as_shot_neutral: Some([0.55, 1.0, 0.7]),
        calibrations: vec![
            ColorCalibration {
                illuminant: Illuminant::StandardA,
                color_matrix: cm_a,
                forward_matrix: Some(color::forward_matrix(&CCM)),
            },
            ColorCalibration {
                illuminant: Illuminant::D65,
                color_matrix: cm_d,
                forward_matrix: Some(color::forward_matrix(&CCM)),
            },
        ],
        baseline_exposure: Some(0.5),
        exposure_time: Some(Duration::from_micros(10_000)),
        iso: Some(250),
        capture_time: Some(UNIX_EPOCH + Duration::from_secs(1_791_000_000)),
        model: "ov9782".into(),
        description: Some("{\"sequence\":7}".into()),
        opcode_list2: bayer_gain_maps(CfaPattern::Bggr, (64, 48), (4, 3), [&r, &ones, &ones]),
        preview: Some(Preview {
            width: 4,
            height: 2,
            rgb: (0..24).collect(),
        }),
        ..DngMetadata::default()
    };
    let bytes = write_dng(&img, &meta).unwrap();
    let d = read_dng(&bytes).unwrap();
    assert_eq!(d.version, [1, 4, 0, 0]);
    assert_eq!((d.width, d.height, d.bits_per_sample), (64, 48, 16));
    assert_eq!(d.samples, img.samples);
    assert_eq!(d.cfa, Some(CfaPattern::Bggr));
    assert_eq!(d.color_at(0, 0), Some(2));
    assert_eq!(d.color_at(1, 1), Some(0));
    assert_eq!(d.black_level, vec![64.0, 64.0, 64.0, 65.5]);
    assert_eq!(d.black_at(1, 1, 0), 65.5);
    assert_eq!(d.black_at(2, 0, 0), 64.0);
    assert_eq!(d.white_level, vec![1023]);
    assert!(close(
        d.as_shot_neutral.as_deref().unwrap(),
        &[0.55, 1.0, 0.7],
        1e-6
    ));
    assert_eq!(d.calibrations.len(), 2);
    assert_eq!(d.calibrations[0].illuminant, 17);
    assert_eq!(d.calibrations[1].illuminant, 21);
    assert!(close(&d.calibrations[0].color_matrix.unwrap(), &cm_a, 1e-4));
    assert!(close(
        &d.calibrations[1].forward_matrix.unwrap(),
        &color::forward_matrix(&CCM),
        1e-4
    ));
    // The matrices give the tuning's CCM and neutral back (to the file's precision).
    let (ccm, n) = color::ccm_from_color_matrix(
        &d.calibrations[1].color_matrix.unwrap(),
        Illuminant::D65.xy(),
    )
    .unwrap();
    assert!(close(&ccm, &CCM, 2e-3), "{ccm:?}");
    assert!(close(&n, &neutral_d65, 1e-3), "{n:?}");
    assert_eq!(d.baseline_exposure, Some(0.5));
    assert_eq!(d.exposure_time, Some(0.01));
    assert_eq!(d.iso, Some(250));
    assert_eq!(d.model.as_deref(), Some("ov9782"));
    assert_eq!(d.make.as_deref(), Some("Styx"));
    assert_eq!(d.unique_camera_model.as_deref(), Some("Styx ov9782"));
    assert_eq!(d.date_time_original.as_deref(), Some("2026:10:03 04:00:00"));
    assert_eq!(d.description.as_deref(), Some("{\"sequence\":7}"));
    let maps = d.gain_maps();
    assert_eq!(maps.len(), 4);
    // Red (BGGR: (1, 1)) gets the red table.
    assert_eq!((maps[0].top, maps[0].left), (1, 1));
    assert!((maps[0].gain_at(1.0, 1.0, 0) - 2.1).abs() < 1e-6);
}

#[test]
fn mono_and_eight_bit_frames_without_a_preview() {
    let samples: Vec<u16> = (0..32 * 8).map(|i| (i % 256) as u16).collect();
    let img = RawImage::new(32, 8, SampleLayout::Mono, 8, samples.clone()).unwrap();
    let meta = DngMetadata {
        black_level: [16.0; 4],
        ..DngMetadata::default()
    };
    let d = read_dng(&write_dng(&img, &meta).unwrap()).unwrap();
    assert_eq!((d.photometric, d.bits_per_sample), (34892, 8));
    assert_eq!(d.samples, samples);
    assert_eq!(d.cfa, None);
    assert_eq!(d.white_level, vec![255]);
    assert_eq!(d.black_at(3, 3, 0), 16.0);
    assert!(d.calibrations.is_empty());
    // Packed RAW10 in, the same samples out.
    let img = bayer(8, 2);
    let mut packed = Vec::new();
    for row in img.samples.chunks(8) {
        for q in row.chunks(4) {
            packed.extend(q.iter().map(|v| (v >> 2) as u8));
            packed.push(
                q.iter()
                    .enumerate()
                    .fold(0u8, |a, (i, v)| a | ((v & 3) as u8) << (2 * i)),
            );
        }
    }
    let from_packed = RawImage::from_packed(
        &packed,
        8,
        2,
        10,
        Packing::Csi2Raw10,
        SampleLayout::Cfa(CfaPattern::Bggr),
    )
    .unwrap();
    assert_eq!(from_packed, img);
}

/// A minimal big-endian TIFF builder for files shaped like cameras' DNGs.
struct BeTiff {
    entries: Vec<(u16, u16, u32, Vec<u8>)>,
    blobs: Vec<Vec<u8>>,
}

impl BeTiff {
    fn shorts(&mut self, tag: u16, v: &[u16]) {
        let d = v.iter().flat_map(|x| x.to_be_bytes()).collect();
        self.entries.push((tag, 3, v.len() as u32, d));
    }
    fn longs(&mut self, tag: u16, v: &[u32]) {
        let d = v.iter().flat_map(|x| x.to_be_bytes()).collect();
        self.entries.push((tag, 4, v.len() as u32, d));
    }
    fn bytes(&mut self, tag: u16, v: &[u8]) {
        self.entries.push((tag, 1, v.len() as u32, v.to_vec()));
    }
    fn srationals(&mut self, tag: u16, v: &[f64]) {
        let d = v
            .iter()
            .flat_map(|x| {
                [
                    ((x * 10000.0).round() as i32).to_be_bytes(),
                    10000i32.to_be_bytes(),
                ]
                .concat()
            })
            .collect();
        self.entries.push((tag, 10, v.len() as u32, d));
    }
    /// Blobs are placed after the IFD; `offsets_tag` gets their offsets.
    fn build(mut self, offsets_tag: u16, counts_tag: u16) -> Vec<u8> {
        let counts: Vec<u32> = self.blobs.iter().map(|b| b.len() as u32).collect();
        self.longs(offsets_tag, &vec![0; counts.len()]);
        self.longs(counts_tag, &counts);
        self.entries.sort_by_key(|e| e.0);
        let n = self.entries.len();
        let mut pos = 8 + 2 + 12 * n + 4;
        let mut extra = Vec::new();
        let mut placed = Vec::new();
        for e in &self.entries {
            if e.3.len() > 4 {
                placed.push(Some(pos + extra.len()));
                extra.extend_from_slice(&e.3);
            } else {
                placed.push(None);
            }
        }
        pos += extra.len();
        let mut blob_offsets = Vec::new();
        for b in &self.blobs {
            blob_offsets.push(pos as u32);
            pos += b.len();
        }
        let mut out = b"MM\0\x2a\0\0\0\x08".to_vec();
        out.extend_from_slice(&(n as u16).to_be_bytes());
        let mut extra = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let data: Vec<u8> = if e.0 == offsets_tag {
                blob_offsets.iter().flat_map(|o| o.to_be_bytes()).collect()
            } else {
                e.3.clone()
            };
            out.extend_from_slice(&e.0.to_be_bytes());
            out.extend_from_slice(&e.1.to_be_bytes());
            out.extend_from_slice(&e.2.to_be_bytes());
            match placed[i] {
                Some(at) => {
                    out.extend_from_slice(&(at as u32).to_be_bytes());
                    extra.extend_from_slice(&data);
                }
                None => {
                    let mut d = data.clone();
                    d.resize(4, 0);
                    out.extend_from_slice(&d);
                }
            }
        }
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&extra);
        for b in &self.blobs {
            out.extend_from_slice(b);
        }
        out
    }
}

fn camera_like(w: u32, h: u32) -> BeTiff {
    let mut t = BeTiff {
        entries: Vec::new(),
        blobs: Vec::new(),
    };
    t.bytes(tag::DNG_VERSION, &[1, 4, 0, 0]);
    t.longs(tag::NEW_SUBFILE_TYPE, &[0]);
    t.longs(tag::IMAGE_WIDTH, &[w]);
    t.longs(tag::IMAGE_LENGTH, &[h]);
    t.shorts(tag::PHOTOMETRIC, &[32803]);
    t.shorts(tag::CFA_REPEAT_PATTERN_DIM, &[2, 2]);
    t.bytes(tag::CFA_PATTERN, &[0, 1, 1, 2]);
    t.shorts(tag::BLACK_LEVEL_REPEAT_DIM, &[1, 1]);
    t.longs(tag::BLACK_LEVEL, &[256]);
    t.longs(tag::WHITE_LEVEL, &[4000]);
    t.srationals(
        tag::COLOR_MATRIX_1,
        &[0.9, -0.3, -0.1, -0.4, 1.2, 0.2, -0.1, 0.2, 0.6],
    );
    t.shorts(tag::CALIBRATION_ILLUMINANT_1, &[21]);
    t
}

#[test]
fn camera_style_files_with_lossless_jpeg_tiles_and_packed_strips() {
    let (w, h) = (40u32, 20u32);
    let samples: Vec<u16> = (0..w * h).map(|i| ((i * 131) % 4096) as u16).collect();
    // Tiles of 16x16 (partial at the edges), each lossless JPEG with two components of half
    // the tile's width, as Adobe's converter writes them.
    let mut t = camera_like(w, h);
    t.shorts(tag::BITS_PER_SAMPLE, &[12]);
    t.shorts(tag::COMPRESSION, &[7]);
    t.longs(tag::TILE_WIDTH, &[16]);
    t.longs(tag::TILE_LENGTH, &[16]);
    for ty in 0..h.div_ceil(16) {
        for tx in 0..w.div_ceil(16) {
            let mut tile = vec![0u16; 256];
            for y in 0..16 {
                for x in 0..16 {
                    let (sx, sy) = (tx * 16 + x, ty * 16 + y);
                    if sx < w && sy < h {
                        tile[(y * 16 + x) as usize] = samples[(sy * w + sx) as usize];
                    }
                }
            }
            t.blobs
                .push(crate::ljpeg::tests::encode(&tile, 8, 16, 2, 12, 1));
        }
    }
    let d = read_dng(&t.build(tag::TILE_OFFSETS, tag::TILE_BYTE_COUNTS)).unwrap();
    assert_eq!(d.samples, samples);
    assert_eq!(d.cfa, Some(CfaPattern::Rggb));
    assert_eq!((d.black_at(5, 5, 0), d.white_level[0]), (256.0, 4000));
    assert_eq!(d.calibrations[0].illuminant, 21);
    assert!((d.calibrations[0].color_matrix.unwrap()[4] - 1.2).abs() < 1e-9);

    // Uncompressed 12-bit samples packed MSB first in two strips, with a linearization table.
    let mut t = camera_like(w, h);
    t.shorts(tag::BITS_PER_SAMPLE, &[12]);
    t.shorts(tag::COMPRESSION, &[1]);
    t.longs(tag::ROWS_PER_STRIP, &[10]);
    let table: Vec<u16> = (0..4096).map(|i| (i * 2) as u16).collect();
    t.shorts(tag::LINEARIZATION_TABLE, &table);
    for strip in samples.chunks((w * 10) as usize) {
        let mut b = Vec::new();
        for pair in strip.chunks(2) {
            b.push((pair[0] >> 4) as u8);
            b.push((((pair[0] & 15) << 4) | (pair[1] >> 8)) as u8);
            b.push((pair[1] & 255) as u8);
        }
        t.blobs.push(b);
    }
    let d = read_dng(&t.build(tag::STRIP_OFFSETS, tag::STRIP_BYTE_COUNTS)).unwrap();
    let doubled: Vec<u16> = samples.iter().map(|v| v * 2).collect();
    assert_eq!(d.samples, doubled);
}

#[test]
fn not_dngs_are_errors() {
    assert!(read_dng(b"").is_err());
    assert!(read_dng(b"II*\0\x08\0\0\0\0\0\0\0\0\0").is_err());
    let mut t = camera_like(4, 4);
    t.shorts(tag::BITS_PER_SAMPLE, &[16]);
    t.shorts(tag::COMPRESSION, &[8]);
    t.blobs.push(vec![0; 32]);
    let e = read_dng(&t.build(tag::STRIP_OFFSETS, tag::STRIP_BYTE_COUNTS)).unwrap_err();
    assert!(matches!(e, DngError::Unsupported(_)), "{e}");
    // Truncated: the strip is cut.
    let bytes = write_dng(&bayer(8, 8), &DngMetadata::default()).unwrap();
    assert!(read_dng(&bytes[..bytes.len() - 10]).is_err());
}

//! Damaged MJPEG frames (USB bit errors, truncated network frames, hostile streams): every
//! decoder must return an error or a picture, never panic, and never size a buffer from a
//! header that claims far more than the stream's resolution.

use styx_core::prelude::*;

use crate::Codec;

const FIXTURE: &[u8] = include_bytes!("../../../testing/fixtures/c270_720p_rst.mjpeg");

fn jpegs() -> Vec<&'static [u8]> {
    let starts: Vec<usize> = FIXTURE
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w == &[0xFF, 0xD8])
        .map(|(i, _)| i)
        .collect();
    starts
        .iter()
        .zip(starts.iter().skip(1).chain(std::iter::once(&FIXTURE.len())))
        .map(|(&start, &end)| &FIXTURE[start..end])
        .take(3)
        .collect()
}

fn mjpeg_frame(jpeg: &[u8]) -> FrameLease {
    let mut buf = BufferPool::with_limits(1, jpeg.len().max(1), 0).lease();
    buf.resize(jpeg.len());
    buf.as_mut_slice().copy_from_slice(jpeg);
    let res = Resolution::new(1280, 720).unwrap();
    FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 0),
        buf,
        jpeg.len(),
        jpeg.len(),
    )
}

/// Every MJPEG decoder compiled in, with how many damaged frames to feed it (the pure-Rust
/// decoders are slow in debug builds).
fn decoders() -> Vec<(Box<dyn Codec>, usize)> {
    #[allow(unused_mut)]
    let mut decoders: Vec<(Box<dyn Codec>, usize)> = Vec::new();
    #[cfg(feature = "codec-turbojpeg")]
    {
        decoders.push((
            Box::new(crate::mjpeg_turbojpeg::TurbojpegDecoder::new(FourCc::RG24)),
            300,
        ));
        decoders.push((
            Box::new(crate::mjpeg_turbojpeg_luma::TurbojpegLumaDecoder::new()),
            300,
        ));
    }
    #[cfg(feature = "codec-jpeg-decoder")]
    decoders.push((Box::new(crate::mjpeg::MjpegDecoder::new(FourCc::RG24)), 20));
    #[cfg(feature = "codec-zune")]
    decoders.push((
        Box::new(crate::mjpeg_zune::ZuneMjpegDecoder::new(FourCc::RG24)),
        20,
    ));
    decoders
}

/// Deterministic xorshift, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn damaged_frames_decode_or_fail_cleanly() {
    let jpegs = jpegs();
    for (decoder, rounds) in decoders() {
        let name = decoder.descriptor().impl_name;
        assert!(decoder.process(mjpeg_frame(jpegs[0])).is_ok(), "{name}");
        let mut rng = Rng(0xC270);
        for round in 0..rounds {
            let mut jpeg = jpegs[round % jpegs.len()].to_vec();
            match round % 4 {
                // Truncated mid-scan or mid-header.
                0 => jpeg.truncate(rng.below(jpeg.len())),
                // Bit errors in the headers (tables, frame size, restart interval).
                1 => {
                    for _ in 0..1 + rng.below(3) {
                        let at = rng.below(jpeg.len().min(700));
                        jpeg[at] ^= 1 << rng.below(8);
                    }
                }
                // Bit errors anywhere.
                2 => {
                    for _ in 0..1 + rng.below(8) {
                        let at = rng.below(jpeg.len());
                        jpeg[at] = rng.next() as u8;
                    }
                }
                // Markers where the entropy-coded data should be.
                _ => {
                    let at = rng.below(jpeg.len().saturating_sub(2));
                    jpeg[at] = 0xFF;
                    jpeg[at + 1] = [0xD0, 0xD9, 0xC0, 0xDA][rng.below(4)];
                }
            }
            let _ = decoder.process(mjpeg_frame(&jpeg));
        }
    }
}

#[test]
fn headers_claiming_huge_pictures_are_refused_before_allocating() {
    let mut jpeg = jpegs()[0].to_vec();
    let sof = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xC0])
        .expect("baseline SOF0");
    // SOF0: marker, length (2), precision (1), height (2), width (2). 8000x8000 (192 MB as
    // RGB) is within every decoder's own limits.
    jpeg[sof + 5..sof + 9].copy_from_slice(&[0x1F, 0x40, 0x1F, 0x40]);
    for (decoder, _) in decoders() {
        let name = decoder.descriptor().impl_name;
        let err = decoder.process(mjpeg_frame(&jpeg)).err();
        assert!(
            err.as_ref()
                .is_some_and(|e| e.to_string().contains("far larger")),
            "{name}: {err:?}"
        );
    }
}

#[cfg(feature = "codec-turbojpeg")]
#[test]
fn slice_splitting_handles_damaged_frames() {
    let jpegs = jpegs();
    let mut rng = Rng(0x511C3);
    for round in 0..2_000 {
        let mut jpeg = jpegs[round % jpegs.len()].to_vec();
        if round % 2 == 0 {
            jpeg.truncate(rng.below(jpeg.len()));
        }
        for _ in 0..rng.below(6) {
            let at = rng.below(jpeg.len());
            jpeg[at] = if rng.below(2) == 0 {
                0xFF
            } else {
                rng.next() as u8
            };
        }
        if let Some(slices) = crate::jpeg_slices::split_jpeg(&jpeg, 1 + rng.below(8)) {
            assert!(!slices.is_empty());
        }
    }
}

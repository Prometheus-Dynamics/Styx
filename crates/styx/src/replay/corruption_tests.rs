//! Damaged recordings: truncated, bit-flipped and overwritten files must read as errors or as
//! fewer frames, never panic, hang or allocate far beyond the file size.

use std::io::Cursor;

use styx_core::prelude::*;

use super::tests::{formats, frame, grey, header, temp_path};
use super::*;

/// A small recording with raw, compressed, NV12 and pyramid-companion frames.
fn sample(format: StreamFormat, test: &str) -> Vec<u8> {
    let path = temp_path(&format!("{test}-{format:?}"));
    let mut recorder = StreamRecorder::with_format(&path, &header(grey(64, 32)), format).unwrap();
    let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
    let mut buffer = BufferPool::with_limits(1, 64, 0).lease();
    buffer.resize(jpeg.len());
    buffer.as_mut_slice().copy_from_slice(&jpeg);
    let mjpeg = FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(
                FourCc::MJPG,
                Resolution::new(64, 32).unwrap(),
                ColorSpace::Srgb,
            ),
            2_000,
        ),
        buffer,
        jpeg.len(),
        jpeg.len(),
    );
    let nv12 = FrameLease::from_visible_bytes(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(64, 32).unwrap(),
            ColorSpace::Bt709,
        ),
        3_000,
        &vec![7; 64 * 32 * 3 / 2],
    )
    .unwrap();
    let pyramid = frame(1, 1_000).with_box_pyramid(1, 1).unwrap();
    for f in [frame(0, 0), pyramid, mjpeg, nv12] {
        recorder.record(&f).unwrap();
    }
    recorder.finish().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(path);
    bytes
}

/// The reader's own buffer ([`open_recording_reader`] reads 1 MiB at a time).
const READ_BUFFER: usize = 1 << 20;

/// Read every frame of `bytes` and return how many were read. `Err` results are fine; panics
/// and allocations beyond the reader's buffer or a few times the file size are not.
fn read_all(bytes: &[u8]) -> usize {
    let (count, largest) = crate::test_alloc::largest_allocation(|| {
        let Ok((_, frames)) =
            open_recording_reader(Cursor::new(bytes.to_vec()), bytes.len() as u64)
        else {
            return 0;
        };
        // Read each frame's pixels too, as a consumer would.
        frames
            .take(64)
            .filter_map(Result::ok)
            .map(|f| drop(f.to_visible_vec()))
            .count()
    });
    assert!(
        largest <= READ_BUFFER.max(4 * bytes.len() + 8192),
        "allocated {largest} bytes reading a {}-byte recording",
        bytes.len()
    );
    count
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
fn every_truncation_reads_without_panicking() {
    for format in formats() {
        let bytes = sample(format, "truncation");
        let full = read_all(&bytes);
        assert_eq!(full, 4, "{format:?}");
        let step = (bytes.len() / 400).max(1);
        for len in (0..bytes.len()).step_by(step) {
            assert!(read_all(&bytes[..len]) <= full, "{format:?} cut at {len}");
        }
    }
}

#[test]
fn random_corruption_reads_without_panicking() {
    for format in formats() {
        let bytes = sample(format, "corruption");
        let mut rng = Rng(0x5eed_0000 ^ bytes.len() as u64);
        for round in 0..1_000 {
            let mut damaged = bytes.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(damaged.len());
                match round % 3 {
                    // Flip one bit.
                    0 => damaged[at] ^= 1 << rng.below(8),
                    // Overwrite a run with 0xFF: huge lengths, sizes and counts.
                    1 => {
                        let end = (at + 1 + rng.below(8)).min(damaged.len());
                        damaged[at..end].fill(0xFF);
                    }
                    // Random byte.
                    _ => damaged[at] = rng.next() as u8,
                }
            }
            read_all(&damaged);
        }
    }
}

#[cfg(feature = "replay-mcap")]
#[test]
fn repeated_pyramid_levels_are_rejected() {
    use super::mcap_format::{decode_meta, encode_meta};
    let meta = frame(0, 0).meta().clone();
    assert!(decode_meta(&encode_meta(&meta, "/styx/image", &[1, 2])).is_ok());
    // Assembling this frame used to panic looking up level 1's metadata a second time.
    assert!(matches!(
        decode_meta(&encode_meta(&meta, "/styx/image", &[1, 1])),
        Err(ReplayError::Corrupt(_))
    ));
}

#[cfg(feature = "replay-mcap")]
#[test]
fn chunk_headers_claiming_huge_fields_are_rejected() {
    let bytes = sample(StreamFormat::Mcap, "chunk-header");
    // Chunk record: opcode 0x06, length u64, start/end time, uncompressed size, CRC, then the
    // compression name length (u32). The mcap crate asks to buffer that many bytes.
    let chunk = bytes
        .windows(9)
        .position(|w| w[0] == 0x06 && u64::from_le_bytes(w[1..9].try_into().unwrap()) < 1 << 20)
        .expect("a chunk record");
    let mut damaged = bytes.clone();
    damaged[chunk + 37..chunk + 41].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
    // `read_all` fails the test if this allocates more than the file.
    read_all(&damaged);
}

/// Writes small recordings of every format to `$STYX_FUZZ_SEEDS/replay_reader/` as seeds for
/// the `replay_reader` fuzz target (`docs/fuzzing.md`).
#[test]
#[ignore = "writes fuzz seeds; run with STYX_FUZZ_SEEDS set"]
fn write_fuzz_seeds() {
    let Some(dir) = std::env::var_os("STYX_FUZZ_SEEDS") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("replay_reader");
    std::fs::create_dir_all(&dir).unwrap();
    for format in formats() {
        std::fs::write(
            dir.join(format!("sample-{format:?}")),
            sample(format, "seed"),
        )
        .unwrap();
        // A small native capture: two 16x8 frames with the sensor's exposure and gains.
        let path = temp_path(&format!("seed-native-{format:?}"));
        let mut recorder =
            StreamRecorder::with_format(&path, &header(grey(16, 8)), format).unwrap();
        for i in 0..2u8 {
            let bytes: Vec<u8> = (0..16 * 8).map(|p| (p as u8).wrapping_add(i)).collect();
            let mut f =
                FrameLease::from_visible_bytes(grey(16, 8), u64::from(i) * 33_000_000, &bytes)
                    .unwrap();
            f.meta_mut().clock = Some(TimestampClock::Monotonic);
            f.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
                sequence: u32::from(i),
                bytes_used: 128,
                error: false,
                exposure_ns: 9_998_000,
                analog_gain: 2.5,
                digital_gain: 1.0,
                frame_duration_ns: 33_333_000,
                frame_length: 3662,
                verified: true,
            }));
            recorder.record(&f).unwrap();
        }
        recorder.finish().unwrap();
        std::fs::copy(&path, dir.join(format!("native-{format:?}"))).unwrap();
        let _ = std::fs::remove_file(path);
    }
}

/// Inputs the `replay_reader` fuzz target found (`fuzz_regressions/`): each must read without
/// panicking or allocating beyond the file.
#[test]
fn fuzz_regressions_read_without_panicking() {
    // A `.styxrec` frame whose format claims ~3.8 EB of pixels: the copy of its visible rows
    // was allocated before its planes were checked.
    read_all(include_bytes!("fuzz_regressions/styxrec-huge-plane"));
}

//! Fuzzed recordings: a deterministic generator mutates headers and records for many iterations.
//! A mutated header is always refused (its checksum covers every byte of it); a mutated record
//! never panics and never yields a frame that fails its own checks. Nothing here panics.

mod support;

use std::io::Cursor;

use styx_record::{Recorder, Replay};

use support::{Prng, fps30, payload_of, samples};

const ITERATIONS: usize = 20_000;

fn recording() -> (Vec<u8>, usize) {
    let frames = samples(13);
    let header = styx_record::RecordingHeader {
        device: "fuzz camera".into(),
        keys: vec!["a".into(), "b".into(), "c".into()],
        backend: "native".into(),
        format: frames[0].entry.format,
        interval: fps30(),
        payload: payload_of(&frames),
    };
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    for f in &frames {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let bytes = recorder.finish().unwrap();
    let header_end = 14 + u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize + 4;
    (bytes, header_end)
}

#[test]
fn mutated_headers_are_always_refused() {
    let (original, header_end) = recording();
    let mut prng = Prng::new(0x5EED_0001);
    let mut refused = 0usize;
    for _ in 0..ITERATIONS {
        let mut bytes = original.clone();
        match prng.below(4) {
            // One to four bytes changed, each by a nonzero amount.
            0 => {
                for _ in 0..=prng.below(4) {
                    let at = prng.below(header_end);
                    bytes[at] ^= 1 + prng.below(255) as u8;
                }
            }
            // A four-byte field overwritten with random bytes (lengths, counts, values).
            1 => {
                let at = prng.below(header_end - 4);
                bytes[at..at + 4].copy_from_slice(&prng.next_u64().to_le_bytes()[..4]);
            }
            // Cut short at a random point.
            2 => bytes.truncate(prng.below(header_end)),
            // Extra bytes appended inside the header's span.
            _ => {
                let at = prng.below(header_end);
                bytes.insert(at, prng.next_u64() as u8);
            }
        }
        if bytes == original {
            continue;
        }
        let result = Replay::new(Cursor::new(bytes));
        assert!(result.is_err(), "a mutated header opened");
        refused += 1;
    }
    assert!(
        refused > ITERATIONS / 2,
        "only {refused} mutations were tested"
    );
}

#[test]
fn mutated_records_never_panic() {
    let (original, header_end) = recording();
    let mut prng = Prng::new(0x5EED_0002);
    let mut frames_seen = 0u64;
    for _ in 0..ITERATIONS {
        let mut bytes = original.clone();
        for _ in 0..=prng.below(6) {
            let at = header_end + prng.below(bytes.len() - header_end);
            bytes[at] = prng.next_u64() as u8;
        }
        if prng.below(5) == 0 {
            let cut = header_end + prng.below(bytes.len() - header_end);
            bytes.truncate(cut);
        }
        // The header is intact, so the recording opens; each record is read or refused.
        let mut replay = Replay::new(Cursor::new(bytes)).expect("the header is intact");
        while let Ok(Some(_)) = replay.next_frame() {
            frames_seen += 1;
        }
    }
    assert!(frames_seen > 0);
}

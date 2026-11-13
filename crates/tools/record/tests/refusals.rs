//! Crafted and damaged recordings are refused: truncated and oversized headers, wrong magic and
//! version, checksums that do not match, and frame layouts that run past their buffer. Records
//! with a valid checksum but a bad value are refused by their checks, not by the checksum.

mod support;

use std::io::Cursor;

use styx_record::{RecordError, Recorder, Replay};

use support::{crc32, fps30, payload_of, samples};

/// A two-frame recording and the offset where its records start.
fn recording() -> (Vec<u8>, usize) {
    let frames = samples(5);
    let header = styx_record::RecordingHeader {
        device: "cam".into(),
        keys: vec!["k".into()],
        backend: "native".into(),
        format: frames[0].entry.format,
        interval: fps30(),
        payload: payload_of(&frames),
    };
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    for f in &frames[..2] {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let bytes = recorder.finish().unwrap();
    let header_end = 14 + u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize + 4;
    (bytes, header_end)
}

/// Replaces the header body's last eight bytes (the payload) and recomputes its checksum.
fn with_payload(mut bytes: Vec<u8>, payload: u64) -> Vec<u8> {
    let len = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
    let body = 14..14 + len;
    bytes[body.end - 8..body.end].copy_from_slice(&payload.to_le_bytes());
    let crc = crc32(&bytes[body.clone()]);
    bytes[body.end..body.end + 4].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// Edits the first frame record's header and recomputes that record's checksum.
fn with_frame_header(
    mut bytes: Vec<u8>,
    header_end: usize,
    edit: impl FnOnce(&mut [u8]),
) -> Vec<u8> {
    assert_eq!(bytes[header_end], 1, "the first record is a frame");
    let len =
        u32::from_le_bytes(bytes[header_end + 1..header_end + 5].try_into().unwrap()) as usize;
    let start = header_end + 5;
    edit(&mut bytes[start..start + len]);
    let crc = crc32(&bytes[start..start + len]);
    bytes[start + len..start + len + 4].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn first_frame_error(bytes: Vec<u8>) -> RecordError {
    let mut replay = Replay::new(Cursor::new(bytes)).expect("header opens");
    replay
        .next_frame()
        .expect_err("the damaged frame is refused")
}

#[test]
fn intact_recording_replays() {
    let (bytes, _) = recording();
    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    assert!(replay.next_frame().unwrap().is_some());
    assert!(replay.next_frame().unwrap().is_some());
    assert!(replay.next_frame().unwrap().is_none());
}

#[test]
fn every_truncated_header_is_refused() {
    let (bytes, header_end) = recording();
    for cut in 0..header_end {
        assert!(
            Replay::new(Cursor::new(bytes[..cut].to_vec())).is_err(),
            "a header cut to {cut} bytes opened"
        );
    }
}

#[test]
fn wrong_magic_and_version_are_refused() {
    let (mut bytes, _) = recording();
    bytes[0] = b'X';
    assert!(matches!(
        Replay::new(Cursor::new(bytes.clone())),
        Err(RecordError::NotARecording)
    ));
    bytes[0] = b'S';
    bytes[8..10].copy_from_slice(&2u16.to_le_bytes());
    assert!(matches!(
        Replay::new(Cursor::new(bytes)),
        Err(RecordError::UnsupportedVersion(2))
    ));
}

#[test]
fn oversized_header_length_is_refused_before_allocation() {
    let (mut bytes, _) = recording();
    bytes[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        Replay::new(Cursor::new(bytes)),
        Err(RecordError::TooLarge("header"))
    ));
}

#[test]
fn damaged_header_fails_its_checksum() {
    let (mut bytes, _) = recording();
    bytes[20] ^= 0x40;
    assert!(matches!(
        Replay::new(Cursor::new(bytes)),
        Err(RecordError::Corrupt("header checksum"))
    ));
}

#[test]
fn bad_payload_values_are_refused_even_with_a_valid_checksum() {
    let (bytes, _) = recording();
    assert!(matches!(
        Replay::new(Cursor::new(with_payload(bytes.clone(), 0))),
        Err(RecordError::Corrupt("zero payload"))
    ));
    assert!(matches!(
        Replay::new(Cursor::new(with_payload(bytes.clone(), 1 << 40))),
        Err(RecordError::TooLarge("payload"))
    ));
    assert!(matches!(
        Replay::new(Cursor::new(with_payload(bytes, u64::MAX))),
        Err(RecordError::TooLarge("payload"))
    ));
}

#[test]
fn oversized_string_in_header_is_refused() {
    let (mut bytes, _) = recording();
    // The device string's length is the first two bytes of the body.
    bytes[14..16].copy_from_slice(&u16::MAX.to_le_bytes());
    let crc =
        crc32(&bytes[14..14 + u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize]);
    let end = 14 + u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
    bytes[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(
        Replay::new(Cursor::new(bytes)),
        Err(RecordError::TooLarge("string"))
    ));
}

#[test]
fn plane_running_past_its_buffer_is_refused() {
    let (bytes, header_end) = recording();
    // Frame header: id 8, timestamp 8, format 14, plane count 1, then per plane offset, stride,
    // len (8 each). The first plane's len is at 31 + 16.
    let crafted = with_frame_header(bytes, header_end, |h| {
        h[47..55].copy_from_slice(&(1u64 << 40).to_le_bytes());
    });
    assert!(matches!(
        first_frame_error(crafted),
        RecordError::Corrupt("plane runs past the frame buffer")
    ));
}

#[test]
fn zero_planes_and_too_many_planes_are_refused() {
    let (bytes, header_end) = recording();
    let zero = with_frame_header(bytes.clone(), header_end, |h| h[30] = 0);
    assert!(matches!(
        first_frame_error(zero),
        RecordError::Corrupt("plane count")
    ));
    let many = with_frame_header(bytes, header_end, |h| h[30] = 9);
    assert!(matches!(
        first_frame_error(many),
        RecordError::Corrupt("plane count")
    ));
}

#[test]
fn damaged_frame_fails_its_checksum() {
    let (mut bytes, header_end) = recording();
    bytes[header_end + 6] ^= 0x01;
    assert!(matches!(
        first_frame_error(bytes),
        RecordError::Corrupt("frame checksum")
    ));
}

#[test]
fn oversized_and_empty_frame_headers_are_refused() {
    let (mut bytes, header_end) = recording();
    bytes[header_end + 1..header_end + 5].copy_from_slice(&(1u32 << 30).to_le_bytes());
    assert!(matches!(
        first_frame_error(bytes.clone()),
        RecordError::TooLarge("frame header")
    ));
    bytes[header_end + 1..header_end + 5].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        first_frame_error(bytes)
            .to_string()
            .contains("frame header")
    );
}

#[test]
fn unknown_record_tag_is_refused() {
    let (mut bytes, header_end) = recording();
    bytes[header_end] = 7;
    assert!(matches!(
        first_frame_error(bytes),
        RecordError::Corrupt("unknown record tag")
    ));
}

#[test]
fn recorder_refuses_what_replay_would_refuse() {
    let frames = samples(9);
    let header = styx_record::RecordingHeader {
        device: "cam".into(),
        keys: vec![],
        backend: "native".into(),
        format: frames[0].entry.format,
        interval: None,
        payload: 16,
    };
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    let f = &frames[0];
    let span = f.entry.layouts.span().unwrap();
    assert!(
        recorder
            .record_frame(&f.entry, &f.meta, &f.data[..span - 1])
            .is_err()
    );
    assert!(
        recorder.record_frame(&f.entry, &f.meta, &f.data).is_err(),
        "larger than the payload"
    );
}

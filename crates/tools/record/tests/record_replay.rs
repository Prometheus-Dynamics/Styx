//! Record then replay: the frames, their identities, timestamps, layouts, metadata and bytes come
//! back exactly, from memory and from a file, and a consumer that holds every buffer has frames
//! dropped and counted.

mod support;

use std::io::Cursor;
use std::path::Path;

use styx_controls::FrameMetadata;
use styx_frame::{BufferSize, FramePool};
use styx_record::{REPLAY_BUFFERS, Recorder, RecordingHeader, Replay};

use support::{Sample, fps30, payload_of, samples};

fn header_for(samples: &[Sample]) -> RecordingHeader {
    RecordingHeader {
        device: "sim camera".into(),
        keys: vec!["sensor-1".into(), "serial-42".into()],
        backend: "native".into(),
        format: samples[0].entry.format,
        interval: fps30(),
        payload: payload_of(samples),
    }
}

/// Records `samples` the way a camera does: each frame is a lease from the producer's pool,
/// written by the producer, then recorded from the lease.
fn record_leases(samples: &[Sample], header: &RecordingHeader) -> Vec<u8> {
    let producer: FramePool<FrameMetadata> =
        FramePool::host(REPLAY_BUFFERS, BufferSize::new(header.payload as usize, 0)).unwrap();
    let mut recorder = Recorder::new(Vec::new(), header).unwrap();
    for sample in samples {
        let mut lease = producer.acquire(sample.entry).unwrap();
        let span = sample.entry.layouts.span().unwrap();
        lease.write().unwrap().bytes()[..span].copy_from_slice(&sample.data);
        *lease.meta_mut().unwrap() = sample.meta;
        recorder.record(&lease).unwrap();
    }
    assert_eq!(recorder.frames(), samples.len() as u64);
    recorder.finish().unwrap()
}

fn assert_same(replayed: &styx_frame::FrameLease<FrameMetadata>, original: &Sample) {
    assert_eq!(replayed.id(), original.entry.id);
    assert_eq!(replayed.timestamp_ns(), original.entry.timestamp_ns);
    assert_eq!(*replayed.entry(), original.entry);
    assert_eq!(*replayed.meta(), original.meta);
    let span = original.entry.layouts.span().unwrap();
    let read = replayed.read().unwrap();
    assert_eq!(
        &read.bytes()[..span],
        &original.data[..],
        "frame bytes differ"
    );
}

#[test]
fn lease_record_then_replay_is_identical() {
    let frames = samples(7);
    let header = header_for(&frames);
    let bytes = record_leases(&frames, &header);

    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    assert_eq!(replay.header(), &header);
    for original in &frames {
        let lease = replay.next_frame().unwrap().expect("a recorded frame");
        assert_same(&lease, original);
    }
    assert!(replay.next_frame().unwrap().is_none());
    assert_eq!(replay.delivered(), frames.len() as u64);
    assert_eq!(replay.dropped(), 0);
    assert!(!replay.truncated());
}

#[test]
fn parts_record_then_replay_is_identical() {
    let frames = samples(11);
    let header = header_for(&frames);
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    for f in &frames {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let bytes = recorder.finish().unwrap();
    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    for original in &frames {
        let lease = replay.next_frame().unwrap().expect("a recorded frame");
        assert_same(&lease, original);
    }
    assert!(replay.next_frame().unwrap().is_none());
}

#[test]
fn file_round_trip() {
    let frames = samples(23);
    let header = header_for(&frames);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("record_replay_round_trip.styxrec");
    let mut recorder = Recorder::create(&path, &header).unwrap();
    for f in &frames {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    recorder.finish().unwrap();

    let mut replay = Replay::open(&path).unwrap();
    for original in &frames {
        let lease = replay.next_frame().unwrap().expect("a recorded frame");
        assert_same(&lease, original);
    }
    assert!(replay.next_frame().unwrap().is_none());
    assert!(!replay.truncated());
}

#[test]
fn held_buffers_drop_frames_and_count_them() {
    let frames = samples(31);
    let header = header_for(&frames);
    // Record every sample twice: eight frames, four buffers.
    let mut doubled = frames.clone();
    doubled.extend(frames.iter().cloned());
    let bytes = record_leases(&doubled, &header);

    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    let mut held = Vec::new();
    while let Some(lease) = replay.next_frame().unwrap() {
        held.push(lease);
    }
    assert_eq!(held.len(), REPLAY_BUFFERS);
    assert_eq!(replay.dropped(), (doubled.len() - REPLAY_BUFFERS) as u64);
    for (lease, original) in held.iter().zip(&doubled) {
        assert_same(lease, original);
    }
}

#[test]
fn recording_cut_short_ends_at_its_last_whole_frame() {
    let frames = samples(41);
    let header = header_for(&frames);
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    for f in &frames {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    // No end marker: the file just stops after the last frame.
    let whole = recorder.finish().unwrap();
    let without_end = whole[..whole.len() - 1].to_vec();

    let mut replay = Replay::new(Cursor::new(without_end.clone())).unwrap();
    for original in &frames {
        assert_same(&replay.next_frame().unwrap().expect("frame"), original);
    }
    assert!(replay.next_frame().unwrap().is_none());
    assert!(
        !replay.truncated(),
        "a file that stops between records is complete"
    );

    // Every cut past the header: whole frames before it replay, the rest is refused as cut short.
    let header_end = 14 + u32::from_le_bytes(without_end[10..14].try_into().unwrap()) as usize + 4;
    for cut in header_end..without_end.len() {
        let mut replay = Replay::new(Cursor::new(without_end[..cut].to_vec()))
            .unwrap_or_else(|e| panic!("cut {cut} is past the header and must open: {e}"));
        let mut count = 0usize;
        while let Some(lease) = replay.next_frame().unwrap() {
            assert_same(&lease, &frames[count]);
            count += 1;
        }
        assert!(count < frames.len(), "cut {cut} kept every frame");
    }
}

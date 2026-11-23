//! Recording and replaying frames with a 1 kHz channel beside them allocate nothing in a steady
//! state: a counting allocator over 2,000 frames and 64,000 samples after warm-up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{self, Cursor, Write};

use styx_controls::FrameMetadata;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_record::{ChannelSpec, Item, Recorder, RecordingHeader, Replay};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; it only counts this thread's allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's contract for `alloc` is passed on as it is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const FRAMES: u64 = 2_100;
const WARM_UP: u64 = 100;
/// 120 fps frames, 1 kHz samples: 8 or 9 samples per frame interval.
const FRAME_NS: u64 = 8_333_333;

/// Counts bytes and keeps none: a writer that allocates nothing.
struct Count(u64);

impl Write for Count {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn setup() -> (RecordingHeader, FrameEntry, Vec<u8>) {
    let (width, height) = (64u32, 48u32);
    let layouts = packed_layouts(FourCc::GREY, width, height, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(width, height).unwrap());
    let span = layouts.span().unwrap();
    let header = RecordingHeader {
        device: "steady".into(),
        keys: vec![],
        backend: "native".into(),
        format,
        interval: None,
        payload: span as u64,
    };
    (
        header,
        FrameEntry::new(0, 0, format, layouts),
        vec![0x5A; span],
    )
}

fn imu() -> ChannelSpec {
    ChannelSpec {
        name: "/imu".into(),
        encoding: "raw".into(),
        ..ChannelSpec::default()
    }
}

/// Frames at 120 fps and 1 kHz samples, each frame written 3 ms after its time (as it reaches
/// a consumer), so samples newer than a frame are written before it.
fn record<W: Write>(recorder: &mut Recorder<W>, entry: FrameEntry, data: &[u8], warm: &mut u64) {
    let imu = recorder.declare(imu()).unwrap();
    let meta = FrameMetadata::default();
    let sample = [7u8; 24];
    let mut next_sample = 1_000_000_000u64;
    for n in 0..FRAMES {
        if n == WARM_UP {
            *warm = allocations();
        }
        let ts = 1_000_000_000 + n * FRAME_NS;
        while next_sample <= ts + 3_000_000 {
            recorder.sample(imu, next_sample, &sample).unwrap();
            next_sample += 1_000_000;
        }
        let entry = FrameEntry {
            id: n,
            timestamp_ns: ts,
            ..entry
        };
        recorder.record_frame(&entry, &meta, data).unwrap();
    }
}

#[test]
fn recording_frames_and_samples_allocates_nothing_after_warm_up() {
    let (header, entry, data) = setup();
    let mut recorder = Recorder::new(Count(0), &header).unwrap();
    let mut warm = 0;
    record(&mut recorder, entry, &data, &mut warm);
    let after = allocations();
    assert!(recorder.samples() > 17_000);
    assert_eq!(after - warm, 0, "allocations while recording");
}

#[test]
fn replaying_frames_and_samples_allocates_nothing_after_warm_up() {
    let (header, entry, data) = setup();
    let mut recorder = Recorder::new(Vec::new(), &header).unwrap();
    let mut ignored = 0;
    record(&mut recorder, entry, &data, &mut ignored);
    let bytes = recorder.finish().unwrap();

    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    let (mut frames, mut samples, mut checksum) = (0u64, 0u64, 0u64);
    let mut warm = 0;
    while let Some(item) = replay.next_item().unwrap() {
        match item {
            Item::Frame(frame) => {
                if frames == WARM_UP {
                    warm = allocations();
                }
                frames += 1;
                checksum += u64::from(frame.read().unwrap().bytes()[0]);
            }
            Item::Sample(s) => {
                samples += 1;
                checksum += u64::from(s.data[0]);
            }
        }
    }
    let after = allocations();
    assert_eq!(frames, FRAMES);
    assert_eq!(replay.late_samples(), 0);
    assert!(samples > 17_000 && checksum > 0);
    assert_eq!(after - warm, 0, "allocations while replaying");
}

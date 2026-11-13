//! The timed proof: a preview at 15 fps beside a main reader of the same source ring at full
//! rate. The main reader must get every frame, and its timing must be what it is without the
//! preview.
//!
//! Run it alone, on an otherwise idle machine:
//! `cargo test -p styx-preview --release --test proof -- --ignored --nocapture`
//!
//! Setup: a source ring of NV12 640x400 at 60 fps for four seconds (240 frames, paced to an
//! absolute schedule on the source thread), 8 buffers. The main reader connects, reads each
//! frame's planes and records its delivery age (`Frame::age_ns`, now minus the frame's stamp) and
//! the gap between consecutive deliveries. With the preview, a second reader of the same ring
//! offers each frame to a `Preview` (15 fps, quality 70, 640x400), and a viewer reads the
//! preview's JPEG ring.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use styx_preview::{Offer, Preview, PreviewConfig, PreviewStats};
use styx_socket::{Frame, Reader, ReaderStats};

use common::{connect, is_jpeg, now_ns, publish, source};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 400;
const SOURCE_FPS_NS: u64 = 16_666_667;
const FRAMES: u64 = 240;

#[derive(Debug, Default)]
struct Main {
    delivered: u64,
    gaps_in_seq: u64,
    ages_ns: Vec<u64>,
    gaps_ns: Vec<u64>,
    reader: Option<ReaderStats>,
}

#[derive(Debug, Default)]
struct Viewer {
    frames: u64,
    jpeg: u64,
    bytes: u64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn run(with_preview: bool) -> (Main, u64, Option<(PreviewStats, Viewer, f64)>) {
    let mut ring = source(WIDTH, HEIGHT, 8);
    let main_reader = connect(&mut ring);
    let done = Arc::new(AtomicBool::new(false));

    let main = thread::spawn({
        let done = Arc::clone(&done);
        move || {
            let mut reader = main_reader;
            let mut out = Main::default();
            let mut last_seq = None;
            let mut last_at: Option<Instant> = None;
            loop {
                match reader.next_frame().expect("main reader") {
                    Some(frame) => {
                        let at = Instant::now();
                        if let Some(prev) = last_at {
                            out.gaps_ns.push((at - prev).as_nanos() as u64);
                        }
                        last_at = Some(at);
                        if let Some(prev) = last_seq {
                            out.gaps_in_seq += frame.seq().saturating_sub(prev + 1);
                        }
                        last_seq = Some(frame.seq());
                        out.ages_ns.push(frame.age_ns());
                        // What a vision consumer does with a frame: read its planes.
                        let read = frame.lease().read().expect("readable");
                        std::hint::black_box(read.plane(0).expect("plane").data[0]);
                        out.delivered += 1;
                    }
                    None if done.load(Ordering::Acquire) => break,
                    None => thread::sleep(Duration::from_micros(100)),
                }
            }
            out.reader = Some(reader.stats());
            out.ages_ns.sort_unstable();
            out.gaps_ns.sort_unstable();
            out
        }
    });

    let preview_side = with_preview.then(|| {
        let preview =
            Preview::<(), Frame>::new(PreviewConfig::new(WIDTH, HEIGHT)).expect("preview");
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("pair");
        preview.attach(ours);
        let viewer = thread::spawn({
            let done = Arc::clone(&done);
            move || {
                let mut reader = Reader::connect(theirs).expect("viewer");
                let mut out = Viewer::default();
                loop {
                    match reader.next_frame().expect("viewer reader") {
                        Some(frame) => {
                            let lease = frame.lease();
                            let read = lease.read().expect("readable");
                            let plane = read.plane(0).expect("plane");
                            out.frames += 1;
                            out.bytes += plane.data.len() as u64;
                            out.jpeg += u64::from(is_jpeg(plane.data));
                        }
                        None if done.load(Ordering::Acquire) => break,
                        None => thread::sleep(Duration::from_micros(200)),
                    }
                }
                out
            }
        });
        let feeder_reader = connect(&mut ring);
        let feeder = thread::spawn({
            let done = Arc::clone(&done);
            move || {
                let mut reader = feeder_reader;
                loop {
                    match reader.next_frame().expect("preview reader") {
                        Some(frame) => {
                            let share = frame.lease().share();
                            if preview.offer_held(frame, &share) == Offer::Refused {
                                panic!("a 640x400 NV12 frame is refused");
                            }
                        }
                        None if done.load(Ordering::Acquire) => break,
                        None => thread::sleep(Duration::from_micros(100)),
                    }
                }
                // The worker still encodes what it holds: give it time, then count.
                thread::sleep(Duration::from_millis(200));
                preview.stats()
            }
        });
        (feeder, viewer)
    });

    // The source: FRAMES frames at 60 fps on an absolute schedule.
    let start = Instant::now();
    let mut published = 0_u64;
    for i in 0..FRAMES {
        let due = start + Duration::from_nanos(i * SOURCE_FPS_NS);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        if publish(&mut ring, WIDTH, HEIGHT, i, now_ns()) {
            published += 1;
        }
    }
    let source_elapsed = start.elapsed();
    thread::sleep(Duration::from_millis(300));
    done.store(true, Ordering::Release);

    let main_out = main.join().expect("main reader thread");
    let preview_out = preview_side.map(|(feeder, viewer)| {
        let stats = feeder.join().expect("feeder");
        let viewed = viewer.join().expect("viewer");
        (stats, viewed, source_elapsed.as_secs_f64())
    });
    let _ = published;
    (main_out, published, preview_out)
}

fn report(label: &str, main: &Main, published: u64) {
    let stats = main.reader.expect("reader stats");
    println!(
        "{label}: source published {published}/{FRAMES}; main delivered {} skipped {} (seq gaps {}); \
         age p50 {:.3} ms p99 {:.3} ms max {:.3} ms; gap p50 {:.3} ms p99 {:.3} ms max {:.3} ms",
        main.delivered,
        stats.skipped,
        main.gaps_in_seq,
        ms(percentile(&main.ages_ns, 0.50)),
        ms(percentile(&main.ages_ns, 0.99)),
        ms(*main.ages_ns.last().unwrap_or(&0)),
        ms(percentile(&main.gaps_ns, 0.50)),
        ms(percentile(&main.gaps_ns, 0.99)),
        ms(*main.gaps_ns.last().unwrap_or(&0)),
    );
}

#[test]
#[ignore = "timed: run alone with --release --ignored --nocapture on an idle machine"]
fn preview_at_fifteen_leaves_the_main_reader_alone() {
    let (base, base_published, _) = run(false);
    report("baseline (no preview)", &base, base_published);
    assert_eq!(base.delivered, base_published, "baseline dropped frames");
    assert_eq!(base.gaps_in_seq, 0);

    let (with, published, preview) = run(true);
    report("with preview at 15 fps", &with, published);
    assert_eq!(with.delivered, published, "main reader missed frames");
    assert_eq!(with.gaps_in_seq, 0, "main reader saw a sequence gap");
    assert_eq!(with.reader.expect("stats").skipped, 0);

    let (stats, viewed, seconds) = preview.expect("preview ran");
    let fps = stats.published as f64 / seconds;
    println!(
        "preview: offered {} rate-capped {} queued-then-replaced {} published {} no-buffer {} failed {} \
         readers {}; viewer got {} frames ({} JPEG, avg {} bytes); preview fps over the run {:.2} (source {:.2} s); \
         encode mean {:.3} ms",
        stats.offered,
        stats.rate_capped,
        stats.replaced,
        stats.published,
        stats.no_buffer,
        stats.failed,
        stats.readers,
        viewed.frames,
        viewed.jpeg,
        viewed.bytes.checked_div(viewed.frames.max(1)).unwrap_or(0),
        fps,
        seconds,
        ms(stats.encode_ns.checked_div(stats.published).unwrap_or(0)),
    );
    assert!(viewed.jpeg > 0 && viewed.jpeg == viewed.frames);
    assert!((13.0..=16.0).contains(&fps), "preview fps {fps}");
}

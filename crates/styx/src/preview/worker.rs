//! The preview thread: takes the latest frame (offered, or from a camera service client),
//! scales it, lets it go, encodes the picture and publishes the JPEG.

use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use bytes::Bytes;
use styx_codec::jpeg_planar::{JpegInput, PlanarJpegEncoder};
use styx_core::prelude::*;

use super::api::{PreviewFrame, Shared};
use super::scale::{self, Scaler};
use crate::ipc::{ClientOptions, FrameClient};

/// Where frames come from.
pub(super) enum Source {
    /// [`Preview::offer`](super::Preview::offer).
    Offered,
    /// A low-priority, reconnecting camera service client, connected while watched.
    Service(ClientOptions),
}

/// Lets frames through at most `fps` times a second, evenly: each accepted frame moves the
/// next due time on by one period (not from when it arrived), so a 30 fps camera capped at 20
/// gives 20, not 15. Behind by more than a period, it starts again from now.
pub(crate) struct RateGate {
    period: Option<Duration>,
    next: Option<Instant>,
}

impl RateGate {
    pub(crate) fn new(fps: f32) -> Self {
        Self {
            period: (fps > 0.0).then(|| Duration::from_secs_f64(1.0 / f64::from(fps))),
            next: None,
        }
    }

    /// Whether a frame arriving `now` goes through (and if so, it counts).
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        let Some(period) = self.period else {
            return true;
        };
        // Arrival jitter: a frame up to an eighth of a period early still counts.
        let slack = period / 8;
        match self.next {
            Some(next) if now + slack < next => false,
            Some(next) if now < next + period => {
                self.next = Some(next + period);
                true
            }
            _ => {
                self.next = Some(now + period);
                true
            }
        }
    }
}

/// CPU time of the calling thread.
fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec to write.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Lower (or raise) the calling thread's priority; refused changes are ignored.
fn renice(nice: i32) {
    // SAFETY: gettid has no preconditions; setpriority on our own thread id.
    unsafe {
        let tid = libc::gettid();
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice);
    }
}

struct Encoder {
    scaler: Scaler,
    jpeg: PlanarJpegEncoder,
    out: Vec<u8>,
    sequence: u64,
}

pub(super) fn run(shared: &Shared, source: Source, jpeg: PlanarJpegEncoder) {
    if let Some(nice) = shared.config.nice {
        renice(nice);
    }
    let mut encoder = Encoder {
        scaler: Scaler::default(),
        jpeg,
        out: Vec::new(),
        sequence: 0,
    };
    match source {
        Source::Offered => offered(shared, &mut encoder),
        Source::Service(options) => service(shared, &options, &mut encoder),
    }
    shared.counters.cpu_ns.store(thread_cpu_ns(), Relaxed);
}

fn offered(shared: &Shared, encoder: &mut Encoder) {
    while !shared.stop.load(Acquire) {
        let frame = {
            let mut input = shared.input.lock();
            if input.is_none() {
                shared
                    .input_ready
                    .wait_for(&mut input, Duration::from_millis(200));
            }
            input.take()
        };
        if let Some(frame) = frame {
            encoder.process(shared, frame);
        }
        let counters = &shared.counters;
        counters
            .subscribers
            .store(shared.output.subscribers.load(Relaxed), Relaxed);
        counters.cpu_ns.store(thread_cpu_ns(), Relaxed);
    }
}

fn service(shared: &Shared, options: &ClientOptions, encoder: &mut Encoder) {
    let request = shared.config.request();
    let counters = &shared.counters;
    let mut client: Option<FrameClient> = None;
    let mut unwatched_since: Option<Instant> = None;
    while !shared.stop.load(Acquire) {
        counters
            .subscribers
            .store(shared.output.subscribers.load(Relaxed), Relaxed);
        if shared.watched() {
            unwatched_since = None;
            if client.is_none() {
                match options.request_nonblocking(&request) {
                    Ok(c) => client = Some(c),
                    Err(err) => {
                        counters.error(format!("camera service client: {err}"));
                        shared.output.wait_watchers(Duration::from_secs(1));
                        continue;
                    }
                }
            }
        } else {
            let since = *unwatched_since.get_or_insert_with(Instant::now);
            if client.is_some() && since.elapsed() >= shared.config.idle_disconnect {
                // Nobody watches: let the camera idle (and the service forget this client).
                client = None;
            }
        }
        let Some(frames) = &client else {
            shared.output.wait_watchers(Duration::from_millis(200));
            continue;
        };
        let mut frame = match frames.recv(Duration::from_millis(100)) {
            RecvOutcome::Data(frame) => frame,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => {
                client = None;
                continue;
            }
        };
        counters.frames_in.fetch_add(1, Relaxed);
        // Only the newest: frames that arrived while encoding are let go at once.
        while let RecvOutcome::Data(newer) = frames.try_next() {
            counters.frames_in.fetch_add(1, Relaxed);
            counters.dropped_busy.fetch_add(1, Relaxed);
            frame = newer;
        }
        if !shared.watched() {
            counters.dropped_unwatched.fetch_add(1, Relaxed);
            continue;
        }
        if !shared.gate.lock().due(Instant::now()) {
            counters.dropped_rate.fetch_add(1, Relaxed);
            continue;
        }
        encoder.process(shared, frame);
        counters.cpu_ns.store(thread_cpu_ns(), Relaxed);
    }
}

impl Encoder {
    /// Scale `frame`, let it go, encode, publish.
    fn process(&mut self, shared: &Shared, frame: FrameLease) {
        let counters = &shared.counters;
        let started = Instant::now();
        let meta = frame.meta();
        let (timestamp_ns, clock) = (meta.timestamp, meta.clock);
        // How old the frame is (capture timestamp to now), when it is on a system clock.
        let age = clock
            .and_then(TimestampClock::now_ns)
            .map(|now| Duration::from_nanos(now.saturating_sub(timestamp_ns)));
        let code = meta.format.code;
        if code == FourCc::MJPG && shared.config.passthrough_jpeg {
            let res = meta.format.resolution;
            let jpeg = frame
                .planes()
                .first()
                .map(|p| Bytes::copy_from_slice(p.data()))
                .unwrap_or_default();
            drop(frame);
            if jpeg.is_empty() {
                counters.error("camera JPEG frame without data".into());
                return;
            }
            counters.passthrough.fetch_add(1, Relaxed);
            let size = (res.width.get(), res.height.get());
            self.publish(
                shared,
                jpeg,
                size,
                false,
                true,
                (timestamp_ns, clock),
                started,
                age,
            );
            return;
        }
        let target = shared.config.max_size;
        let src = scale::source(&frame, target);
        let res = src.meta().format.resolution;
        let (src_w, src_h) = (res.width.get(), res.height.get());
        let size = scale::fit((src_w, src_h), target);
        let scaled = self.scaler.scale(src, size);
        // The camera buffer goes back now: encoding works on the preview's own copy.
        drop(frame);
        counters.scale.push_duration(started.elapsed());
        if let Err(err) = scaled {
            counters.error(err);
            return;
        }
        counters.source_width.store(u64::from(src_w), Relaxed);
        counters.source_height.store(u64::from(src_h), Relaxed);
        let encode_started = Instant::now();
        let picture = &self.scaler.picture;
        let (w, h) = (picture.width, picture.height);
        let input = if picture.gray {
            JpegInput::Gray {
                y: &picture.y,
                width: w,
                height: h,
                stride: w,
            }
        } else {
            JpegInput::I420 {
                y: &picture.y,
                u: &picture.u,
                v: &picture.v,
                width: w,
                height: h,
                y_stride: w,
                c_stride: w / 2,
            }
        };
        let gray = picture.gray;
        if let Err(err) = self.jpeg.encode(&input, &mut self.out) {
            counters.error(err.to_string());
            return;
        }
        counters.encode.push_duration(encode_started.elapsed());
        let jpeg = Bytes::copy_from_slice(&self.out);
        let size = (w as u32, h as u32);
        self.publish(
            shared,
            jpeg,
            size,
            gray,
            false,
            (timestamp_ns, clock),
            started,
            age,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn publish(
        &mut self,
        shared: &Shared,
        jpeg: Bytes,
        (width, height): (u32, u32),
        gray: bool,
        passthrough: bool,
        (timestamp_ns, clock): (u64, Option<TimestampClock>),
        started: Instant,
        age: Option<Duration>,
    ) {
        let counters = &shared.counters;
        let took = started.elapsed();
        self.sequence += 1;
        counters.encoded.fetch_add(1, Relaxed);
        counters.bytes.fetch_add(jpeg.len() as u64, Relaxed);
        counters.jpeg_bytes.push(jpeg.len() as u64);
        counters.width.store(u64::from(width), Relaxed);
        counters.height.store(u64::from(height), Relaxed);
        if let Some(age) = age {
            counters.latency.push_duration(age + took);
        }
        shared.output.publish(PreviewFrame {
            jpeg,
            sequence: self.sequence,
            timestamp_ns,
            clock,
            width,
            height,
            gray,
            passthrough,
            encode_us: u32::try_from(took.as_micros()).unwrap_or(u32::MAX),
        });
    }
}

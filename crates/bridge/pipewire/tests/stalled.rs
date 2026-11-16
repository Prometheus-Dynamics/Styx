//! A consumer that stalls sees a skipped frame, never a torn one.
//!
//! The consumer here is this test's own PipeWire stream, with its process callback on PipeWire's
//! data thread (`RT_PROCESS`), which is where a consumer takes buffers from the link. On every
//! fifth buffer it stops inside that callback, holding the buffer, until the producer has tried a
//! set number of further frames. While it is stopped:
//!
//! - the buffer it holds must not change (it is read before and after, and must be one frame id
//!   both times, the same one), though the producer tries more frames than the ring has buffers;
//! - the buffers the node sends meanwhile reach the link but are not taken, since nothing runs
//!   the consumer's side of it. A sent buffer stays on the link, takeable, until the node's next
//!   send to the same io area replaces it, however many cycles that is. What the consumer takes
//!   when it runs again must be a whole frame it has not seen, and must stay whole while it
//!   holds it: every buffer is read when taken and again after the producer has tried another
//!   frame. It can be an older frame than the one before (the link has two areas, and the
//!   consumer reads them in turn); it is never a torn one.
//!
//! The stall is counted in the producer's frames, not in time: the consumer waits on the
//! producer's own count. A short stall (2 frames) is a consumer that overruns its cycle; a long
//! one (12 frames, three times the ring) is a consumer stopped for many cycles. The
//! graph cycles that passed are read from the graph clock and printed; when the clock did not
//! move, this consumer was the graph's driver and the whole graph stood still, which is the same
//! stall seen from the node: no cycle passes, so the node frees nothing.
//!
//! Needs this build's PipeWire node and a running PipeWire daemon with a session manager.
//! Without them the test prints `SKIPPED` and why.

#![cfg(all(target_os = "linux", feature = "pipewire"))]

mod common;

use std::io::Cursor;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{Rig, no_node_or_daemon};
use pipewire::spa::param::ParamType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value, object, property};
use pipewire::spa::sys::{SPA_IO_Position, spa_io_position};
use pipewire::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pipewire::stream::{StreamBox, StreamFlags};
use pipewire::{context::ContextRc, keys, main_loop::MainLoopRc, properties::properties};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const LUMA: usize = (WIDTH * HEIGHT) as usize;
/// The buffers the consumer takes before the proof ends.
const BUFFERS: usize = 30;
/// Every `EVERY`th buffer is held through a stall.
const EVERY: usize = 5;

/// One buffer as the consumer saw it.
#[derive(Debug, Clone, Copy)]
struct Taken {
    id: u8,
    /// The producer's frames tried while this buffer was held, 0 when it was not held.
    stalled: u64,
    /// The graph cycles that passed meanwhile, by the graph clock.
    cycles: u64,
}

/// What the consumer's callback and the test share.
struct Shared {
    attempts: Arc<AtomicU64>,
    /// How many of the producer's frames a stall lasts.
    stall: u64,
    taken: Mutex<Vec<Taken>>,
    /// The first thing that was wrong with a buffer, if anything was.
    fault: Mutex<Option<String>>,
    done: AtomicBool,
    /// Set by the test to end a stall early when it gives up.
    give_up: AtomicBool,
    /// The graph's position area, null when the stream has none.
    position: AtomicPtr<spa_io_position>,
}

impl Shared {
    fn fail(&self, why: String) {
        let mut fault = self.fault.lock().expect("fault");
        if fault.is_none() {
            *fault = Some(why);
        }
        self.done.store(true, Ordering::Release);
    }

    /// The graph clock's position and the length of a cycle on it, or `None` with no clock.
    fn clock(&self) -> Option<(u64, u64)> {
        let position = self.position.load(Ordering::Acquire);
        if position.is_null() {
            return None;
        }
        // SAFETY: the area is the one PipeWire gave this stream and has not taken back (the
        // pointer is cleared in `io_changed` when it does). The driver writes it from another
        // thread, so each word is read volatile and used only as a count.
        unsafe {
            Some((
                ptr::addr_of!((*position).clock.position).read_volatile(),
                ptr::addr_of!((*position).clock.duration).read_volatile(),
            ))
        }
    }
}

/// The one id every luma byte of the frame at `luma` holds, or how it is not one.
///
/// # Safety
/// `luma` points at `LUMA` readable bytes. Another process may be writing them: each is read
/// volatile.
unsafe fn one_id(luma: *const u8) -> Result<u8, String> {
    // SAFETY: the caller's contract.
    let first = unsafe { luma.read_volatile() };
    for at in 1..LUMA {
        // SAFETY: `at` is inside the `LUMA` bytes of the caller's contract.
        let byte = unsafe { luma.add(at).read_volatile() };
        if byte != first {
            return Err(format!("id {first} up to byte {at}, then {byte}"));
        }
    }
    Ok(first)
}

/// Takes one buffer on PipeWire's data thread, and on every `EVERY`th holds it through a stall.
fn on_process(stream: &pipewire::stream::Stream, shared: &Arc<Shared>) {
    if shared.done.load(Ordering::Acquire) {
        return;
    }
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
    let Some(mapped) = data.data() else {
        return shared.fail("a buffer with no mapped memory".into());
    };
    if size < LUMA || mapped.len() < offset + size {
        return shared.fail(format!(
            "a chunk of {size} bytes at {offset} in {} mapped",
            mapped.len()
        ));
    }
    let luma = mapped[offset..].as_ptr();
    // SAFETY: `luma` is `size >= LUMA` bytes inside the buffer's mapping (checked above), which
    // stays mapped while this callback holds the buffer.
    let id = match unsafe { one_id(luma) } {
        Ok(id) => id,
        Err(how) => return shared.fail(format!("a torn frame when taken: {how}")),
    };
    let count = shared.taken.lock().expect("taken").len();
    // Every buffer is held until the producer has tried one more frame, and every `EVERY`th
    // through a stall of `stall` frames: this thread is the consumer's side of the link, and it
    // does nothing meanwhile. The sleep is only how often it looks at the producer's count.
    let stall = shared.stall > 0 && count % EVERY == EVERY - 1;
    let wait = if stall { shared.stall } else { 1 };
    let from = shared.attempts.load(Ordering::Acquire);
    let clock = shared.clock();
    while shared.attempts.load(Ordering::Acquire) < from + wait
        && !shared.give_up.load(Ordering::Acquire)
    {
        thread::sleep(Duration::from_millis(1));
    }
    let (mut stalled, mut cycles) = (0, 0);
    {
        let held = shared.attempts.load(Ordering::Acquire) - from;
        if stall {
            stalled = held;
            if let (Some((before, _)), Some((after, duration))) = (clock, shared.clock()) {
                cycles = after.wrapping_sub(before) / duration.max(1);
            }
        }
        let stalled = held;
        // SAFETY: as above; the buffer is still held.
        match unsafe { one_id(luma) } {
            Ok(after) if after == id => {}
            Ok(after) => {
                return shared.fail(format!(
                    "the held frame {id} became frame {after} during a stall of {stalled} frames"
                ));
            }
            Err(how) => {
                return shared.fail(format!(
                    "the held frame {id} was written over during a stall of {stalled} frames: {how}"
                ));
            }
        }
    }
    let mut taken = shared.taken.lock().expect("taken");
    taken.push(Taken {
        id,
        stalled,
        cycles,
    });
    if taken.len() >= BUFFERS {
        shared.done.store(true, Ordering::Release);
    }
}

/// Runs the consumer on `node` until it is done or the test gives up. Returns why it could not
/// run, if it could not.
fn consume(node: &str, shared: &Arc<Shared>) -> Result<(), String> {
    pipewire::init();
    let mainloop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context = ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect: {e}"))?;
    let stream = StreamBox::new(
        &core,
        "styx-stalled-consumer",
        properties! {
            *keys::MEDIA_TYPE => "Video",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "Camera",
            "target.object" => node,
        },
    )
    .map_err(|e| format!("stream: {e}"))?;
    let _listener = stream
        .add_local_listener_with_user_data(Arc::clone(shared))
        .io_changed(|_, shared, id, area, size| {
            if id == SPA_IO_Position {
                let whole = size as usize >= size_of::<spa_io_position>();
                let area = if whole { area.cast() } else { ptr::null_mut() };
                shared.position.store(area, Ordering::Release);
            }
        })
        .process(|stream, shared| on_process(stream, shared))
        .register()
        .map_err(|e| format!("listener: {e}"))?;
    let format = object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::VideoFormat, Id, VideoFormat::NV12),
        property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: WIDTH,
                height: HEIGHT
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(format))
        .map_err(|e| format!("format: {e:?}"))?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or("format pod")?];
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect the stream: {e}"))?;
    let timer = {
        let (shared, quit) = (Arc::clone(shared), mainloop.clone());
        mainloop.loop_().add_timer(move |_| {
            if shared.done.load(Ordering::Acquire) || shared.give_up.load(Ordering::Acquire) {
                quit.quit();
            }
        })
    };
    let tick = Duration::from_millis(5);
    let _ = timer.update_timer(Some(tick), Some(tick));
    mainloop.run();
    Ok(())
}

fn proof(name: &str, stall: u64) {
    if let Some(reason) = no_node_or_daemon() {
        println!("SKIPPED: {reason}");
        return;
    }
    let rig = Rig::start(name, WIDTH, HEIGHT, Duration::from_millis(10));
    let shared = Arc::new(Shared {
        attempts: rig.attempts.clone(),
        stall,
        taken: Mutex::new(Vec::new()),
        fault: Mutex::new(None),
        done: AtomicBool::new(false),
        give_up: AtomicBool::new(false),
        position: AtomicPtr::new(ptr::null_mut()),
    });
    let consumer = {
        let (shared, node) = (Arc::clone(&shared), rig.name.clone());
        thread::spawn(move || consume(&node, &shared))
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !shared.done.load(Ordering::Acquire) && !consumer.is_finished() {
        if Instant::now() >= deadline {
            shared.give_up.store(true, Ordering::Release);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let ran = consumer.join().expect("consumer thread");
    let (attempts, no_buffer) = (
        rig.attempts.load(Ordering::Acquire),
        rig.no_buffer.load(Ordering::Acquire),
    );
    let status = rig.control.status();
    let stats = rig.finish();
    ran.expect("the consumer ran");
    let taken = shared.taken.lock().expect("taken").clone();
    if let Some(fault) = shared.fault.lock().expect("fault").take() {
        panic!("{fault}\ntaken so far: {taken:?}");
    }
    assert!(
        taken.len() >= BUFFERS,
        "the consumer took {} of {BUFFERS} buffers: {status:?}\n{taken:?}",
        taken.len()
    );
    // No frame is seen twice: a repeat is a stale buffer handed over again.
    let mut ids: Vec<u8> = taken.iter().map(|t| t.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), taken.len(), "a frame seen twice: {taken:?}");
    // A step forward of more than one is skipped frames; a step back is an older frame handed
    // over after a newer one (whole, as checked when it was taken and again before it was let go).
    let (mut skipped, mut back) = (0_u64, Vec::new());
    for pair in taken.windows(2) {
        let step = pair[1].id.wrapping_sub(pair[0].id);
        if (1..=127).contains(&step) {
            skipped += u64::from(step) - 1;
        } else {
            back.push((pair[0].id, pair[1].id));
        }
    }
    let stalls: Vec<&Taken> = taken.iter().filter(|t| t.stalled > 0).collect();
    let expected = if stall == 0 { 0 } else { BUFFERS / EVERY };
    assert_eq!(stalls.len(), expected, "{taken:?}");
    assert!(stalls.iter().all(|t| t.stalled >= stall), "{taken:?}");
    // The ring is four buffers and the consumer holds one, so of the frames the producer tried
    // during a stall at most three can still be waiting when it ends: the rest are skipped.
    let least = stalls.len() as u64 * stall.saturating_sub(4);
    assert!(
        skipped >= least,
        "{skipped} frames skipped over {} stalls of {stall}, at least {least} expected: {taken:?}",
        stalls.len()
    );
    let cycles: Vec<u64> = stalls.iter().map(|t| t.cycles).collect();
    println!(
        "proof: a consumer stalled {} times for {stall} frames each took {} whole frames, \
         {skipped} skipped, none torn or seen twice, older-after-newer {back:?}; graph cycles per stall \
         {cycles:?}; the producer \
         tried {attempts} frames and found no buffer free for {no_buffer}; node {stats:?}",
        stalls.len(),
        taken.len()
    );
}

/// A plain PipeWire consumer that maps the buffers itself (`MAP_BUFFERS`, as most applications
/// do) and never stalls. PipeWire's stream refuses buffers whose sizes differ, which GStreamer's
/// `pipewiresrc` does not check: this is the consumer that shows it.
#[test]
fn a_consumer_that_maps_the_buffers_itself_reads_whole_frames() {
    proof("styx-map-buffers", 0);
}

/// A consumer that overruns its cycle: stopped for two of the producer's frames.
#[test]
fn a_consumer_that_overruns_its_cycle_sees_whole_frames() {
    proof("styx-stall-short", 2);
}

/// A consumer stopped for three times the ring: twelve of the producer's frames.
#[test]
fn a_consumer_stalled_past_the_whole_ring_sees_whole_frames() {
    proof("styx-stall-long", 12);
}

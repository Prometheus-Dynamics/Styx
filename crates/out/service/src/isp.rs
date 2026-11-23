//! A camera's processed stream: raw frames through an ISP, published beside the raw stream.
//!
//! With `isp` set in a camera's config (see [`crate::config::CameraConfig::isp`]), each attempt at
//! the camera runs one more thread, the camera's ISP thread. The capture thread offers it each raw
//! frame through a hand-off of at most [`HANDOFF`] frames (see [`hold_for`]: fewer when the
//! camera's pool is small); a frame that finds the hand-off full is skipped for the ISP and counted
//! (`isp_skipped`), and the raw socket still publishes it. The status says how many frames got a
//! picture against how many were asked for (`isp_made`), and while frames go without one, why,
//! with the ISP's measured time a frame against the camera's interval (`isp_short`). The ISP thread
//! makes pictures with the PiSP back end ([`styx_pisp::LiveIsp`]) or the soft ISP
//! ([`styx_soft::SoftIspStage`]) and publishes output 0 on `<camera socket>.isp0.sock` and, when
//! `isp_second` is set, output 1 on `.isp1.sock`.
//!
//! - **The copy.** The back end does not read CSI-2 packed raw, so each raw frame is put into a
//!   back-end input buffer in one pass ([`styx_pisp::input`]: an 8-bit row copy, or RAW10
//!   unpacked to 16-bit), counted at site `pisp-be-input` with the bytes read and written. The
//!   outputs are not copied: they are published from the back end's own buffers.
//! - **One producer per output across a switch.** Each output's pool holds the back end's output
//!   buffers (mirrored by descriptor, read-only) and the soft ISP's own buffers, so a picture from
//!   either goes out on the same producer and its readers stay connected when the camera switches
//!   between them. A back-end buffer stays held (its lease kept here) until the producer and its
//!   readers have let the frame go.
//! - **Which ISP made a frame.** Each frame's metadata says so ([`MadeBy`]); the producer's state
//!   page carries the ISP in force, the switches, the refusals and the last reason with its time;
//!   the camera's status line carries the same.
//! - **3A.** With `aaa = true` the camera's 3A loop runs on this thread after each frame is
//!   published (see `aaa`): its statistics come from the back end's input pass or the soft ISP's
//!   own, its settings go to the next frame, and its exposure and gain to the capture thread.
//!   Without it, exposure and gain stay as set and the ISP runs neutral settings (unity white
//!   balance, identity colour matrix, the sRGB curve).
//!
//! With `isp = "auto"` the [`Switcher`] picks the ISP per frame: a frame the back end refuses is
//! made by the soft ISP, so its readers lose no frame to the refusal.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use styx_controls::{FrameMetadata, MadeBy, WIRE_BYTES};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{
    BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, Layouts, MemoryKind,
};
use styx_lemnos::{Client, DeviceEntry};
use styx_pisp::back_end::image_layouts;
use styx_pisp::{Ended, Geometry, InputForm, LiveIsp, Output, PispError};
use styx_settings::IspSettings;
use styx_socket::{Accepted, HandshakePlan, IspActive, IspState, Producer};
use styx_soft::{Options, SoftIspStage, StatsConfig};

use crate::config::{IspMode, IspPlan};
use crate::isp_switch::{Route, Switcher};
use crate::slot::Slot;

/// Raw frames waiting for the ISP thread at most.
pub(crate) const HANDOFF: usize = 2;
/// The fewest frames older than the newest that readers of the raw socket must be able to hold
/// while the ISP runs; a pool that cannot leave them that many refuses `isp`.
pub(crate) const RAW_READERS_MIN: usize = 1;

/// The ISP's share of a camera's pool: the hand-off depth, the reserve the raw producer keeps with
/// the ISP's frames in it, and what that leaves raw readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IspHold {
    /// Raw frames that may wait for the ISP (1 or 2).
    pub depth: usize,
    /// The raw producer's reserve: the camera's own (`base`) plus the ISP's raw frames (the
    /// hand-off and the one it works on).
    pub reserve: usize,
    /// Frames older than the newest that raw readers together may hold
    /// (`buffers - reserve - 1`).
    pub readers: usize,
}

/// The ISP's share for a pool of `buffers` whose camera keeps `base` (the reserve without the ISP,
/// `worker::reserve_for`): the deepest hand-off, up to [`HANDOFF`], that still leaves raw readers
/// [`RAW_READERS_MIN`] frames. `Err` names `isp` and the numbers when even a hand-off of one
/// leaves them fewer: the service refuses the processed stream rather than starve the raw one.
pub(crate) fn hold_for(buffers: usize, base: usize) -> Result<IspHold, String> {
    // readers = buffers - (base + depth + 1) - 1 >= RAW_READERS_MIN
    let room = buffers.saturating_sub(base + 2 + RAW_READERS_MIN);
    let depth = room.min(HANDOFF);
    if depth == 0 {
        return Err(format!(
            "isp refused: the camera's pool has {buffers} buffers and keeps {base}; the ISP needs \
             2 more (a hand-off of 1 and the frame in hand), which would leave raw readers \
             {} frames, fewer than {RAW_READERS_MIN}",
            buffers.saturating_sub(base + 3)
        ));
    }
    let reserve = base + depth + 1;
    Ok(IspHold {
        depth,
        reserve,
        readers: buffers - reserve - 1,
    })
}
/// The soft ISP's buffers per output.
const SOFT_BUFFERS: usize = 4;
/// How long a back-end job may take before the back end is counted as stalled.
const JOB_TIMEOUT: Duration = Duration::from_millis(500);
/// The ring's history, in entries, for each processed socket.
const HISTORY: u32 = 8;
/// Output rows are aligned to this many bytes, for the back end and the soft ISP alike: the back
/// end's own alignment (its input's too), so the size each output needs is the size the device
/// gives for it.
const STRIDE_ALIGN: u32 = 16;
/// How long the ISP thread waits for a frame before it looks at its readers again.
const FRAME_WAIT: Duration = Duration::from_millis(20);

/// A reader on a processed socket, routed to the ISP thread.
#[derive(Debug)]
pub(crate) enum IspSocket {
    /// A reader asks for its handshake plan on output `output`; `None` when nothing is published.
    Handshake {
        output: usize,
        reply: Sender<Option<(u64, HandshakePlan)>>,
    },
    /// A reader answered its handshake: attach it to that output's producer of `planned`.
    Attach {
        output: usize,
        planned: u64,
        accepted: Accepted,
    },
}

/// The capture thread's side: offers raw frames to the ISP thread.
#[derive(Debug)]
pub(crate) struct Feed {
    frames: SyncSender<FrameLease<FrameMetadata>>,
    /// Raw frames the ISP holds now: waiting in the hand-off or in hand.
    outstanding: Arc<AtomicUsize>,
    /// The hand-off depth for this attempt ([`hold_for`]); 0 until set, and while refused.
    depth: AtomicUsize,
    /// Why `isp` is refused for this attempt ([`Feed::refuse`]); the ISP thread reports it and ends.
    refused: Arc<Mutex<Option<String>>>,
}

impl Feed {
    /// Refuses `isp` for this attempt: nothing is offered, and the ISP thread puts `why` in the
    /// status and ends. The raw stream is unaffected.
    pub(crate) fn refuse(&self, why: String) {
        self.depth.store(0, Ordering::Release);
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
    }

    /// Sets the hand-off depth once the pool is known; 0 sends the ISP nothing.
    pub(crate) fn set_depth(&self, depth: usize) {
        self.depth.store(depth.min(HANDOFF), Ordering::Release);
    }

    /// Offers `raw` to the ISP. A full hand-off (the depth waiting, plus the frame in hand) skips
    /// it for the ISP (counted). Nothing is offered while the depth is 0.
    pub(crate) fn offer(&self, raw: &FrameLease<FrameMetadata>, slot: &Slot) {
        let depth = self.depth.load(Ordering::Acquire);
        if depth == 0 {
            return;
        }
        if self.outstanding.load(Ordering::Acquire) > depth {
            slot.isp_skipped();
            return;
        }
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        match self.frames.try_send(raw.share()) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
            }
            Err(TrySendError::Full(_)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                slot.isp_skipped();
            }
        }
    }
}

/// The hand-off between the capture thread and the ISP thread.
pub(crate) fn handoff() -> (Feed, Frames) {
    let (frames, rx) = std::sync::mpsc::sync_channel(HANDOFF);
    let outstanding = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(Mutex::new(None));
    (
        Feed {
            frames,
            outstanding: Arc::clone(&outstanding),
            depth: AtomicUsize::new(0),
            refused: Arc::clone(&refused),
        },
        Frames {
            rx,
            outstanding,
            refused,
        },
    )
}

/// The ISP thread's side of the hand-off. Each raw frame taken counts as held until it is let go
/// ([`Frames::done`]).
#[derive(Debug)]
pub(crate) struct Frames {
    rx: Receiver<FrameLease<FrameMetadata>>,
    outstanding: Arc<AtomicUsize>,
    refused: Arc<Mutex<Option<String>>>,
}

impl Frames {
    /// Why `isp` was refused for this attempt, once.
    fn refused(&self) -> Option<String> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn recv_timeout(&self, wait: Duration) -> Result<FrameLease<FrameMetadata>, RecvTimeoutError> {
        self.rx.recv_timeout(wait)
    }

    /// The ISP let a raw frame go.
    fn done(&self, raw: FrameLease<FrameMetadata>) {
        drop(raw);
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the ISP thread needs from its camera.
pub(crate) struct Context<'a> {
    pub plan: IspPlan,
    pub slot: &'a Slot,
    pub lemnos_socket: Option<PathBuf>,
    /// The camera's ISP devices, in link order (native cameras; empty for the others).
    pub isps: Vec<DeviceEntry>,
    /// The policy's clock (the service's, injected in tests).
    pub now: &'a (dyn Fn() -> Duration + Sync),
    /// Where the 3A loop's tuning is searched first (the config's `tuning_dirs`).
    pub tuning_dirs: &'a [PathBuf],
}

/// The ISP thread: runs until the capture thread drops its [`Feed`].
pub(crate) fn run(ctx: &Context<'_>, frames: &Frames) {
    let (tx, sockets) = std::sync::mpsc::channel();
    ctx.slot.set_isp_route(Some(tx));
    ctx.slot.isp_starting(ctx.plan.mode);
    let mut stage: Option<Stage> = None;
    let mut down = false;
    loop {
        if let Some(why) = frames.refused() {
            ctx.slot.isp_down(&why, styx_socket::clock_ns());
            break;
        }
        serve_readers(&sockets, stage.as_mut());
        match frames.recv_timeout(FRAME_WAIT) {
            Ok(raw) => {
                if stage.is_none() && !down {
                    match Stage::open(ctx, &raw) {
                        Ok(opened) => stage = Some(opened),
                        Err(reason) => {
                            // No processed stream for this attempt: the reason is the status.
                            down = true;
                            ctx.slot.isp_down(&reason, styx_socket::clock_ns());
                        }
                    }
                }
                if let Some(stage) = stage.as_mut() {
                    stage.frame(ctx, &raw);
                }
                frames.done(raw);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    ctx.slot.set_isp_route(None);
    if let Some(stage) = stage {
        stage.close();
    }
    ctx.slot.isp_stopped();
}

fn serve_readers(sockets: &Receiver<IspSocket>, mut stage: Option<&mut Stage>) {
    loop {
        match sockets.try_recv() {
            Ok(IspSocket::Handshake { output, reply }) => {
                let answer = stage
                    .as_deref()
                    .and_then(|s| s.outs.iter().find(|o| o.k == output))
                    .and_then(|o| {
                        o.producer
                            .handshake_plan()
                            .ok()
                            .map(|plan| (o.generation, plan))
                    });
                let _ = reply.send(answer);
            }
            Ok(IspSocket::Attach {
                output,
                planned,
                accepted,
            }) => {
                if let Some(out) = stage
                    .as_deref_mut()
                    .and_then(|s| s.outs.iter_mut().find(|o| o.k == output))
                    && out.generation == planned
                {
                    let _ = out.producer.attach(accepted);
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

/// One published output: its pool (the back end's buffers, then the soft ISP's), its producer,
/// and the back-end leases held while their frames are out.
struct Out {
    k: usize,
    media: MediaFormat,
    layouts: Layouts,
    pool: FramePool<()>,
    be: usize,
    soft: usize,
    shadow: Vec<Option<FrameLease<()>>>,
    producer: Producer<()>,
    generation: u64,
}

impl Out {
    /// Gives back each back-end buffer whose frame the producer and its readers have let go.
    fn release(&mut self) {
        for (i, held) in self.shadow.iter_mut().enumerate() {
            if held.is_some() && self.pool.is_free(i) {
                *held = None;
            }
        }
    }

    fn entry(&self, raw: &FrameLease<FrameMetadata>) -> FrameEntry {
        FrameEntry::new(raw.id(), raw.timestamp_ns(), self.media, self.layouts)
    }

    fn publish(&mut self, lease: FrameLease<()>, blob: &[u8], slot: &Slot) {
        let dropped = self.producer.publish_with(lease, blob).is_err();
        self.producer.reclaim();
        let stats = self.producer.stats();
        slot.isp_published(self.k, dropped, stats.readers);
    }
}

/// Why a back-end frame did not complete.
enum BeMiss {
    /// No free buffer: nothing was posted.
    NotPosted,
    /// The back end refused the job or could not take the frame, for this reason.
    Refused(String),
    /// The back end stopped answering: the session is closed.
    Broken(String),
}

/// The ISP thread's state once the first raw frame has said the format.
struct Stage {
    mode: IspMode,
    settings: IspSettings,
    raw: MediaFormat,
    client: Option<Client>,
    geometry: Option<Geometry>,
    live: Option<LiveIsp<FrameMetadata>>,
    soft: Option<SoftIspStage>,
    switcher: Option<Switcher>,
    outs: Vec<Out>,
    blob: [u8; WIRE_BYTES],
    /// The ISP that made the last frame, for the status.
    active: IspActive,
    refused: u64,
    reason: String,
    reason_at_ns: u64,
    /// What the state page last carried: (active, to_soft, to_pisp, refused, reason time).
    shown: Option<(IspActive, u64, u64, u64, u64)>,
    /// The 3A loop (`aaa`), and the settings the soft ISP has now (it takes new ones only when
    /// they moved: see `aaa::moved`).
    aaa: Option<crate::aaa::Aaa>,
    soft_applied: IspSettings,
    /// The ISP's pace against the camera's, for `isp_short`.
    pace: Pace,
    /// Whether the status shows a soft fallback now (`isp_fallback`).
    fallback_shown: bool,
    /// The settings the status shows left out now (`isp_left_out`).
    left_out_shown: styx_pisp::LeftOut,
}

/// Frames without a picture keep `isp_short` set for this many frames after the last one.
const SHORT_HOLD: u32 = 240;
/// While short, `isp_short` is written again every this many frames (its figures move).
const SHORT_REFRESH: u32 = 30;

/// The ISP's measured time a frame and the camera's frame interval, both smoothed, and the
/// frames since one went without a picture: what `isp_short` says.
#[derive(Debug, Default)]
struct Pace {
    /// The last raw frame's (id, timestamp ns).
    last: Option<(u64, u64)>,
    interval_ns: f64,
    work_ns: f64,
    /// Frames since the last one without a picture (`None`: none went without one yet).
    since_short: Option<u32>,
    text: String,
}

impl Pace {
    /// Smooths over about 16 frames.
    const WEIGHT: f64 = 1.0 / 16.0;

    fn smooth(into: &mut f64, value: f64) {
        *into = if *into == 0.0 {
            value
        } else {
            *into + (value - *into) * Self::WEIGHT
        };
    }

    /// One raw frame `id` taken at `at_ns`, `work_ns` in the ISP, `made` or not. Returns what
    /// `isp_short` should become when it changes: `Some(Some(text))`, or `Some(None)` to clear.
    fn frame(
        &mut self,
        id: u64,
        at_ns: u64,
        work_ns: u64,
        made: bool,
        isp: &str,
    ) -> Option<Option<&str>> {
        // Frames between this one and the last never reached the ISP (the hand-off was full).
        let mut missed = 0;
        if let Some((last_id, last_ns)) = self.last
            && id > last_id
        {
            missed = id - last_id - 1;
            if at_ns > last_ns {
                Self::smooth(
                    &mut self.interval_ns,
                    (at_ns - last_ns) as f64 / (id - last_id) as f64,
                );
            }
        }
        self.last = Some((id, at_ns));
        Self::smooth(&mut self.work_ns, work_ns as f64);
        let short_now = missed > 0 || !made;
        let since = match (short_now, self.since_short) {
            (true, _) => 0,
            (false, Some(n)) => n.saturating_add(1),
            (false, None) => return None,
        };
        self.since_short = Some(since);
        if since > SHORT_HOLD {
            self.since_short = None;
            return Some(None);
        }
        if since != 0 && since % SHORT_REFRESH != 0 && !self.text.is_empty() {
            return None;
        }
        use std::fmt::Write as _;
        let (work, interval) = (self.work_ns / 1e6, self.interval_ns / 1e6);
        self.text.clear();
        if interval > 0.0 && work > interval {
            let _ = write!(
                self.text,
                "{isp} ISP takes {work:.1} ms a frame and frames come every {interval:.1} ms, so about 1 frame in {:.1} gets a picture",
                work / interval
            );
        } else {
            let _ = write!(
                self.text,
                "{isp} ISP takes {work:.1} ms a frame and frames come every {interval:.1} ms; frames went without a picture at the hand-off or for want of a free buffer"
            );
        }
        Some(Some(&self.text))
    }
}

/// The output media and layouts a plan gives for raw frames of `raw`'s size: output 0, and
/// output 1 (the configured one, or half size in output 0's format for the back end's second
/// output, which is then not published).
fn outputs(plan: &IspPlan, raw: &MediaFormat) -> Result<[Output; 2], String> {
    let (w, h) = (raw.resolution.width.get(), raw.resolution.height.get());
    let media = |code, (w, h): (u32, u32)| {
        Resolution::new(w, h)
            .map(|r| MediaFormat::with_defaults(code, r))
            .ok_or_else(|| format!("isp: output size {w}x{h} is empty"))
    };
    let o0 = media(plan.output0.code, plan.output0.size.unwrap_or((w, h)))?;
    let o1 = match plan.output1 {
        Some(spec) => media(spec.code, spec.size.unwrap_or((w / 2, h / 2)))?,
        None => media(plan.output0.code, ((w / 2).max(2), (h / 2).max(2)))?,
    };
    Ok([o0, o1].map(|media| Output {
        media,
        stride_align: STRIDE_ALIGN,
    }))
}

fn layouts_of(output: &Output) -> Result<Layouts, String> {
    let image = output.image().map_err(|e| format!("isp: {e}"))?;
    image_layouts(&image).map_err(|e| format!("isp: {e}"))
}

/// Whether the soft ISP makes `media` from raw frames of `raw`'s size: full or half size, in a
/// format it writes.
fn soft_makes(media: &MediaFormat, raw: &MediaFormat) -> bool {
    let (w, h) = (raw.resolution.width.get(), raw.resolution.height.get());
    let (ow, oh) = (media.resolution.width.get(), media.resolution.height.get());
    let size = (ow == w && oh == h) || (ow == w / 2 && oh == h / 2);
    let code = matches!(
        media.code.to_string().as_str(),
        "RGB3" | "GREY" | "NV12" | "YU12" | "I420"
    );
    size && code
}

/// A read-only mirror of buffer `index` of `pool`, by descriptor, for another pool.
fn mirror(pool: &FramePool<()>, index: usize) -> Result<Box<dyn BufferMemory>, String> {
    let memory = pool
        .memory(index)
        .ok_or_else(|| format!("isp: back-end buffer {index} is missing"))?;
    let fd: BorrowedFd<'_> = memory
        .fd()
        .ok_or_else(|| format!("isp: back-end buffer {index} has no descriptor"))?;
    let owned = fd
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("isp: back-end buffer {index}: {e}"))?;
    let (offset, len) = (memory.fd_offset(), memory.len());
    let mapped = match memory.kind() {
        MemoryKind::Memfd => FdMemory::memfd_at(owned, offset, len, false),
        MemoryKind::Dmabuf => FdMemory::dmabuf_at(owned, offset, len, false),
        other => return Err(format!("isp: back-end buffer {index} is {other:?} memory")),
    };
    mapped
        .map(|m| Box::new(m) as Box<dyn BufferMemory>)
        .map_err(|e| format!("isp: back-end buffer {index}: {e}"))
}

fn soft_buffer(len: usize) -> Result<Box<dyn BufferMemory>, String> {
    let fd = shm_ring::sealed_memfd(len).map_err(|e| format!("isp: soft buffer: {e}"))?;
    FdMemory::memfd(fd, len, true)
        .map(|m| Box::new(m) as Box<dyn BufferMemory>)
        .map_err(|e| format!("isp: soft buffer: {e}"))
}

impl Stage {
    /// Sets the ISP up for frames like `raw`, or says why there is no processed stream.
    fn open(ctx: &Context<'_>, raw: &FrameLease<FrameMetadata>) -> Result<Self, String> {
        let plan = ctx.plan;
        let format = *raw.format();
        let settings = IspSettings::neutral(0.0);
        let outputs = outputs(&plan, &format)?;
        let published = 1 + usize::from(plan.output1.is_some());
        let now_ns = nanos((ctx.now)());

        // The soft ISP, unless the plan asks for the back end alone.
        let soft = if plan.mode == IspMode::Pisp {
            Err("isp = \"pisp\": no soft fallback".to_string())
        } else if let Some(o) = outputs[..published]
            .iter()
            .find(|o| !soft_makes(&o.media, &format))
        {
            Err(format!(
                "soft ISP: makes full or half size in RGB3, GREY, NV12 or I420, not {} {}x{}",
                o.media.code, o.media.resolution.width, o.media.resolution.height
            ))
        } else {
            SoftIspStage::new(
                format.code,
                format.resolution.width.get(),
                format.resolution.height.get(),
                &settings,
                soft_options(
                    ctx.slot.settings.aaa == Some(true),
                    format.resolution.width.get(),
                    format.resolution.height.get(),
                ),
            )
            .map_err(|e| format!("soft ISP: {e}"))
        };

        // The back end, unless the plan asks for the soft ISP alone.
        let mut client = None;
        let mut geometry = None;
        let be = if plan.mode == IspMode::Soft {
            Err("isp = \"soft\"".to_string())
        } else {
            match back_end_geometry(&format, &outputs) {
                Ok(g) => {
                    geometry = Some(g);
                    open_back_end(ctx, &format, &g).map(|(c, live)| {
                        client = Some(c);
                        live
                    })
                }
                Err(e) => Err(e),
            }
        };

        let (live, soft, switcher, active, reason) = match (plan.mode, be, soft) {
            (IspMode::Pisp, Ok(live), _) => (Some(live), None, None, IspActive::Pisp, None),
            (IspMode::Pisp, Err(why), _) => return Err(why),
            (IspMode::Soft, _, Ok(soft)) => (None, Some(soft), None, IspActive::Soft, None),
            (IspMode::Soft, _, Err(why)) => return Err(why),
            (_, Ok(live), Ok(soft)) => (
                Some(live),
                Some(soft),
                Some(Switcher::new(plan.policy)),
                IspActive::Pisp,
                None,
            ),
            // The back end without a soft fallback: it runs, and a refused frame is skipped.
            (_, Ok(live), Err(why)) => (Some(live), None, None, IspActive::Pisp, Some(why)),
            (_, Err(why), Ok(soft)) => (
                None,
                Some(soft),
                Some(Switcher::starting_soft(plan.policy, now_ns, &why)),
                IspActive::Soft,
                Some(why),
            ),
            (_, Err(be), Err(soft)) => return Err(format!("{be}; {soft}")),
        };

        let aaa = (ctx.slot.settings.aaa == Some(true)).then(|| {
            // The config's `tuning_dirs` first, then the system's search (`STYX_TUNING_PATH`,
            // `/etc/styx/tuning`, the bundled tunings).
            let env = std::env::var_os(styx_tuning::TUNING_PATH_ENV);
            let search = styx_tuning::TuningSearch::new(
                ctx.tuning_dirs
                    .iter()
                    .cloned()
                    .chain(env.iter().flat_map(std::env::split_paths))
                    .filter(|p| !p.as_os_str().is_empty())
                    .chain([PathBuf::from(styx_tuning::ADDED_DIR)]),
                Some(PathBuf::from(styx_tuning::BUNDLED_DIR)),
            );
            let found = search.find(ctx.slot.settings.tuning.as_deref());
            crate::aaa::Aaa::new(found.tuning.clone(), found.describe())
        });
        let mut stage = Self {
            mode: plan.mode,
            soft_applied: settings.clone(),
            pace: Pace::default(),
            fallback_shown: false,
            left_out_shown: styx_pisp::LeftOut::default(),
            aaa,
            settings,
            raw: format,
            client,
            geometry,
            live,
            soft,
            switcher,
            outs: Vec::new(),
            blob: [0; WIRE_BYTES],
            active,
            refused: 0,
            reason: String::new(),
            reason_at_ns: 0,
            shown: None,
        };
        stage.build_outs(&outputs[..published])?;
        stage.count_statistics();
        if let Some(why) = reason {
            stage.note(&why);
        }
        if let Some(live) = &stage.live
            && let Some(form) = live.input_form()
        {
            ctx.slot.isp_form(form.name());
        }
        stage.show(ctx.slot);
        Ok(stage)
    }

    /// One producer per published output, over the back end's buffers and the soft ISP's.
    fn build_outs(&mut self, outputs: &[Output]) -> Result<(), String> {
        self.outs.clear();
        for (k, output) in outputs.iter().enumerate() {
            let layouts = layouts_of(output)?;
            let span = layouts
                .span()
                .ok_or_else(|| format!("isp: output {k} has no size"))?;
            let mut memories: Vec<Box<dyn BufferMemory>> = Vec::new();
            let mut be = 0;
            if let Some(pool) = self.live.as_ref().and_then(|l| l.output_pool(k)) {
                for i in 0..pool.buffers() {
                    memories.push(mirror(&pool, i)?);
                }
                be = pool.buffers();
            }
            let soft = if self.soft.is_some() { SOFT_BUFFERS } else { 0 };
            for _ in 0..soft {
                memories.push(soft_buffer(span)?);
            }
            let pool = FramePool::new(BufferSize::new(span, 0), memories)
                .map_err(|e| format!("isp: output {k} pool: {e:?}"))?;
            let buffers = pool.buffers();
            let mut producer = Producer::with_pool(pool.clone(), HISTORY)
                .map_err(|e| format!("isp: output {k} producer: {e}"))?;
            // The ring and its readers together hold at most one buffer less than the smaller
            // part of the pool, so the ISP in force always has one to write.
            let smaller = [be, soft].into_iter().filter(|&n| n > 0).min().unwrap_or(1);
            let hold = smaller.saturating_sub(1).max(1);
            let reserve = producer.set_reserve(buffers.saturating_sub(hold));
            producer.set_keep(crate::worker::keep_for(buffers, reserve));
            let generation = crate::worker::next_generation();
            self.outs.push(Out {
                k,
                media: output.media,
                layouts,
                pool,
                be,
                soft,
                shadow: (0..be).map(|_| None).collect(),
                producer,
                generation,
            });
        }
        Ok(())
    }

    fn note(&mut self, reason: &str) {
        self.reason.clear();
        self.reason.push_str(reason);
        self.reason_at_ns = styx_socket::clock_ns();
    }

    /// The status and every output's state page, when something changed.
    fn show(&mut self, slot: &Slot) {
        let (to_soft, to_pisp) = self.switcher.as_ref().map_or((0, 0), Switcher::switches);
        let now = (
            self.active,
            to_soft,
            to_pisp,
            self.refused,
            self.reason_at_ns,
        );
        if self.shown == Some(now) {
            return;
        }
        self.shown = Some(now);
        let state = IspState::new(
            self.active,
            (to_soft, to_pisp),
            self.refused,
            &self.reason,
            self.reason_at_ns,
        );
        for out in &mut self.outs {
            out.producer.set_isp(&state);
        }
        slot.isp_state(
            self.active,
            (to_soft, to_pisp),
            self.refused,
            &self.reason,
            self.reason_at_ns,
        );
    }

    /// One raw frame through the ISP the policy picks.
    fn frame(&mut self, ctx: &Context<'_>, raw: &FrameLease<FrameMetadata>) {
        for out in &mut self.outs {
            out.release();
        }
        let now = nanos((ctx.now)());
        let started = styx_socket::clock_ns();
        let route = match (self.mode, self.switcher.as_mut()) {
            (_, Some(switcher)) => switcher.route(now),
            (IspMode::Soft, None) => Route::Soft,
            (_, None) => Route::Pisp,
        };
        let made = match route {
            Route::Soft => self.soft_frame(raw, ctx.slot),
            Route::Pisp | Route::PispTry => {
                if self.live.is_none() && route == Route::PispTry {
                    self.reopen(ctx);
                }
                match self.be_frame(raw, ctx.slot) {
                    Ok(()) => {
                        if let Some(switcher) = self.switcher.as_mut() {
                            switcher.completed(now);
                            if let Some((reason, _)) = switcher.last_reason()
                                && reason != self.reason
                            {
                                let reason = reason.to_string();
                                self.note(&reason);
                            }
                        }
                        true
                    }
                    Err(BeMiss::NotPosted) => {
                        if let Some(switcher) = self.switcher.as_mut() {
                            switcher.try_not_posted();
                        }
                        // Made by the soft ISP when there is one, so the stream keeps the frame.
                        self.soft.is_some() && self.soft_frame(raw, ctx.slot)
                    }
                    Err(BeMiss::Refused(reason) | BeMiss::Broken(reason)) => {
                        self.refused += 1;
                        self.note(&reason);
                        if let Some(switcher) = self.switcher.as_mut() {
                            switcher.refused(now, &reason);
                        }
                        ctx.slot.isp_refused();
                        self.soft.is_some() && self.soft_frame(raw, ctx.slot)
                    }
                }
            }
        };
        let work = styx_socket::clock_ns().saturating_sub(started);
        ctx.slot.isp_frame(made);
        let isp = if self.active == IspActive::Pisp {
            "pisp"
        } else {
            "soft"
        };
        if let Some(short) = self
            .pace
            .frame(raw.id(), raw.timestamp_ns(), work, made, isp)
        {
            ctx.slot.isp_short(short);
        }
        if made {
            self.show_fallback_and_left_out(ctx.slot, route);
            self.aaa_step(ctx, raw);
        }
        self.show(ctx.slot);
    }

    /// `isp_fallback` and `isp_left_out`, when they change: a fallback of `isp = "auto"` to the
    /// soft ISP is shown with the back end's refusal for as long as it lasts, and the settings the
    /// ISP in force leaves out are named.
    fn show_fallback_and_left_out(&mut self, slot: &Slot, route: Route) {
        // On the switcher's soft route (or a refused try back): a frame the back end could not
        // take now (busy) and the soft ISP made is not a fallback.
        let falling_back =
            self.mode == IspMode::Auto && route != Route::Pisp && self.active == IspActive::Soft;
        if falling_back != self.fallback_shown {
            self.fallback_shown = falling_back;
            if falling_back {
                let mut why = String::from("the soft ISP makes the frames because ");
                if self.reason.is_empty() {
                    why.push_str("the back end is not open");
                } else {
                    why.push_str(&self.reason);
                }
                slot.isp_fallback(Some(&why));
            } else {
                slot.isp_fallback(None);
            }
        }
        let left_out = match (self.active, self.live.as_ref()) {
            (IspActive::Pisp, Some(live)) => live.left_out(),
            _ => styx_pisp::LeftOut::default(),
        };
        if left_out != self.left_out_shown {
            self.left_out_shown = left_out;
            if left_out.is_empty() {
                slot.isp_left_out(None);
            } else {
                slot.isp_left_out(Some(&left_out.to_string()));
            }
        }
    }

    /// With the 3A loop, has the back end count raw statistics in its input pass (at the black
    /// level of the settings in force).
    fn count_statistics(&mut self) {
        if self.aaa.is_none() {
            return;
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let config = live
            .input_form()
            .and_then(|form| form.stats_config(&self.raw).ok());
        live.count_statistics(config.map(|c| styx_aaa::BayerStats::new(c, 0)));
        set_stats_black(live, &self.settings);
    }

    /// The 3A loop's step on the frame just published, when its ISP gathered statistics: the
    /// settings for the next frame, and the loop's exposure and gain left for the capture thread.
    fn aaa_step(&mut self, ctx: &Context<'_>, raw: &FrameLease<FrameMetadata>) {
        let Some(aaa) = self.aaa.as_mut() else {
            return;
        };
        let counted = match self.active {
            IspActive::Pisp => match self.live.as_ref().and_then(|l| l.raw_statistics(raw.id())) {
                Some(stats) => {
                    stats.finish_into(&mut aaa.stats);
                    true
                }
                None => false,
            },
            IspActive::Soft => aaa.soft_counted,
            _ => false,
        };
        aaa.soft_counted = false;
        if counted {
            let fps = ctx.slot.settings.fps_milli.unwrap_or(0);
            if aaa.step(&ctx.slot.aaa, raw.id(), raw.meta(), fps, &mut self.settings) {
                if let Some(soft) = self.soft.as_mut()
                    && crate::aaa::moved(&self.soft_applied, &self.settings)
                {
                    match soft.apply(&self.settings) {
                        Ok(()) => self.soft_applied.clone_from(&self.settings),
                        Err(e) => {
                            self.reason.clear();
                            self.reason.push_str("soft ISP: settings: ");
                            self.reason.push_str(&e.to_string());
                            self.reason_at_ns = styx_socket::clock_ns();
                        }
                    }
                }
                if let Some(live) = self.live.as_mut() {
                    set_stats_black(live, &self.settings);
                }
            }
        }
        ctx.slot.set_aaa(aaa.status());
    }

    /// Opens the back end again for a try, after it was lost (or never opened).
    fn reopen(&mut self, ctx: &Context<'_>) {
        let Some(geometry) = self.geometry else {
            return;
        };
        match open_back_end(ctx, &self.raw, &geometry) {
            Ok((client, live)) => {
                self.client = Some(client);
                self.live = Some(live);
                self.count_statistics();
                if let Ok(outputs) = outputs(&ctx.plan, &self.raw) {
                    let published = self.outs.len();
                    // New back-end buffers: the producers are made again (readers reconnect).
                    if let Err(why) = self.build_outs(&outputs[..published]) {
                        self.live = None;
                        self.note(&why);
                    }
                }
            }
            Err(why) => self.note(&why),
        }
    }

    /// The frame through the back end, published on every output. The raw lease stays held
    /// until the job ends, so a refused frame can still be made by the soft ISP.
    fn be_frame(&mut self, raw: &FrameLease<FrameMetadata>, slot: &Slot) -> Result<(), BeMiss> {
        let Some(live) = self.live.as_mut() else {
            return Err(BeMiss::Refused("the back end is not open".to_string()));
        };
        let exposure = f64::from(raw.meta().exposure_us);
        match live.post(raw.id(), &self.settings, exposure, raw) {
            Ok(()) => {}
            Err(PispError::Busy(_)) => return Err(BeMiss::NotPosted),
            Err(e) => return Err(BeMiss::Refused(format!("pisp: {e}"))),
        }
        let job = match live.wait_any() {
            Ok(Some(Ended::Done(job))) => job,
            Ok(Some(Ended::Refused { reason, .. })) => {
                return Err(BeMiss::Refused(format!("pisp: job ended: {reason}")));
            }
            Ok(None) => return Err(BeMiss::Refused("pisp: the job was not in flight".into())),
            Err(e) => {
                // A back end that stopped answering is closed; a later try opens it again.
                if let Some(live) = self.live.take() {
                    let _ = live.stop();
                }
                return Err(BeMiss::Broken(format!("pisp: {e}")));
            }
        };
        let len = raw.meta().to_bytes(&mut self.blob);
        MadeBy::Pisp.mark(&mut self.blob[..len]);
        let [o0, o1] = job.outputs;
        for (k, lease) in [(0, o0), (1, o1)] {
            let Some(out) = self.outs.iter_mut().find(|o| o.k == k) else {
                continue;
            };
            let index = lease.buffer_index();
            match out.pool.acquire_at(index, out.entry(raw)) {
                Ok(published) => {
                    out.shadow[index] = Some(lease);
                    out.publish(published, &self.blob[..len], slot);
                }
                Err(_) => slot.isp_published(k, true, out.producer.stats().readers),
            }
        }
        self.active = IspActive::Pisp;
        Ok(())
    }

    /// The frame through the soft ISP, published on every output. `false` when it was not made.
    fn soft_frame(&mut self, raw: &FrameLease<FrameMetadata>, slot: &Slot) -> bool {
        let Some(stage) = self.soft.as_mut() else {
            return false;
        };
        let len = raw.meta().to_bytes(&mut self.blob);
        MadeBy::Soft.mark(&mut self.blob[..len]);
        let mut made = false;
        // The 3A loop's statistics come with the first picture made (one per frame).
        let mut stats = self.aaa.as_mut().map(|aaa| &mut aaa.stats);
        let mut counted = false;
        for out in &mut self.outs {
            let Some(index) = (out.be..out.be + out.soft).find(|&i| out.pool.is_free(i)) else {
                slot.isp_published(out.k, true, out.producer.stats().readers);
                continue;
            };
            let Ok(mut lease) = out.pool.acquire_at(index, out.entry(raw)) else {
                continue;
            };
            let gather = if counted { None } else { stats.as_deref_mut() };
            let gathering = gather.is_some();
            if let Err(e) = stage.process::<FrameMetadata, ()>(raw, &mut lease, gather) {
                // The reason is shown; the frame is not published.
                drop(lease);
                self.reason.clear();
                self.reason.push_str("soft ISP: ");
                self.reason.push_str(&e.to_string());
                self.reason_at_ns = styx_socket::clock_ns();
                continue;
            }
            out.publish(lease, &self.blob[..len], slot);
            counted |= gathering;
            made = true;
        }
        if made {
            self.active = IspActive::Soft;
        }
        if let Some(aaa) = self.aaa.as_mut() {
            aaa.soft_counted = counted;
        }
        made
    }

    fn close(mut self) {
        self.outs.clear();
        if let Some(live) = self.live.take() {
            let _ = live.stop();
        }
    }
}

/// The soft ISP's choices: with the 3A loop, the statistics it meters, on the back end's grid
/// (32x32 zones, every other quad row) so either route gives the loop the same zones; fewer on a
/// frame with fewer quads than that.
fn soft_options(aaa: bool, width: u32, height: u32) -> Options {
    Options {
        stats: aaa.then(|| StatsConfig {
            zones_x: (width / 2).clamp(1, 32),
            zones_y: (height / 2).clamp(1, 32),
            histogram_bins: 128,
            row_step: 2,
            ..StatsConfig::default()
        }),
        ..Options::default()
    }
}

/// The raw statistics' black level: the settings' (normalised), in the counted samples' units.
fn set_stats_black(live: &mut LiveIsp<FrameMetadata>, settings: &IspSettings) {
    if let Some(stats) = live.statistics_mut() {
        let max = f64::from((1u32 << stats.config().bits) - 1);
        stats.set_black((settings.black_level.clamp(0.0, 1.0) * max).round() as u32);
    }
}

/// The back end's geometry for raw frames of `raw`'s format and these outputs.
fn back_end_geometry(raw: &MediaFormat, outputs: &[Output; 2]) -> Result<Geometry, String> {
    let form = InputForm::of(raw).map_err(|e| format!("pisp: {e}"))?;
    Ok(Geometry {
        input: form.image(raw).map_err(|e| format!("pisp: {e}"))?,
        bayer: InputForm::bayer(raw).map_err(|e| format!("pisp: {e}"))?,
        outputs: *outputs,
        tdn: None,
        slice_at: None,
    })
}

/// Opens the first of the camera's ISPs that takes the geometry, with its input form set.
fn open_back_end(
    ctx: &Context<'_>,
    raw: &MediaFormat,
    geometry: &Geometry,
) -> Result<(Client, LiveIsp<FrameMetadata>), String> {
    if ctx.isps.is_empty() {
        return Err("pisp: the camera's chain has no ISP".to_string());
    }
    let socket = ctx
        .lemnos_socket
        .as_ref()
        .ok_or("pisp: no lemnos_socket in the config")?;
    let client = Client::connect(socket, crate::worker::CLIENT_NAME)
        .map_err(|e| format!("pisp: lemnos: {e}"))?;
    let mut refusals = Vec::new();
    for device in &ctx.isps {
        match LiveIsp::open(&client, device, geometry, JOB_TIMEOUT) {
            Ok(mut live) => match live.set_input_form(raw) {
                Ok(_) => return Ok((client, live)),
                Err(e) => {
                    let _ = live.stop();
                    refusals.push(format!("{}: {e}", device.name));
                }
            },
            Err(e) => refusals.push(format!("{}: {e}", device.name)),
        }
    }
    Err(format!("pisp: {}", refusals.join("; ")))
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod hold_tests {
    use super::*;
    use crate::worker::{default_reserve, reserve_for};

    /// The camera's own reserve for a native stream of `buffers`: the declared hold is Lemnos's
    /// `consumer_hold` (the producer keeps half, at least 2).
    fn native_base(buffers: usize) -> usize {
        let hold = styx_lemnos::consumer_hold(buffers as u32);
        reserve_for(buffers, default_reserve(buffers), Some(hold))
    }

    #[test]
    fn the_raze_receivers_nine_buffers_take_a_hand_off_of_one_and_leave_raw_readers_one_frame() {
        // B = 9: Lemnos's producer keeps 4, Styx may hold 5; the camera keeps 9 - 5 + 1 = 5.
        assert_eq!(native_base(9), 5);
        let hold = hold_for(9, 5).expect("fits");
        assert_eq!(
            hold,
            IspHold {
                depth: 1,
                reserve: 7,
                readers: 1
            }
        );
        // Everything Styx holds stays within the declared 5: the frame in hand, the ISP's 2,
        // the ring and its readers' 2 (the newest and one older).
        assert_eq!(1 + (hold.depth + 1) + (9 - hold.reserve), 5);
    }

    #[test]
    fn a_pool_too_small_for_a_raw_reader_refuses_isp_by_name_and_a_large_one_takes_the_full_hand_off()
     {
        // B = 8 (native): the camera keeps 5; the ISP's 2 would leave raw readers 0.
        let base = native_base(8);
        assert_eq!(base, 5);
        let err = hold_for(8, base).expect_err("too small");
        assert!(err.starts_with("isp refused"), "{err}");
        assert!(
            err.contains("8 buffers") && err.contains("keeps 5"),
            "{err}"
        );
        // B = 12: the camera keeps 7; a hand-off of 2, raw readers keep 1.
        assert_eq!(
            hold_for(12, native_base(12)),
            Ok(IspHold {
                depth: 2,
                reserve: 10,
                readers: 1
            })
        );
        // B = 16: the hand-off stays at 2 and raw readers keep the rest.
        assert_eq!(
            hold_for(16, native_base(16)).map(|h| (h.depth, h.readers)),
            Ok((2, 3))
        );
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// The board's case: 120 fps asked (8.3 ms apart), the soft ISP with a full tuning at 16.6 ms
    /// a frame, so every other frame never reaches it. `isp_short` says so with both figures, and
    /// clears once every frame gets a picture again.
    #[test]
    fn a_slow_isp_says_how_slow_against_the_frame_interval() {
        let mut pace = Pace::default();
        let interval = 8_333_333;
        let mut said = None;
        for k in 0..40u64 {
            let id = 2 * k + 1;
            if let Some(Some(text)) = pace.frame(id, id * interval, 16_600_000, true, "soft") {
                said = Some(text.to_string());
            }
        }
        let said = said.expect("isp_short set while frames are missed");
        assert!(
            said.starts_with("soft ISP takes 16.6 ms a frame and frames come every 8.3 ms")
                && said.contains("about 1 frame in 2.0"),
            "{said}"
        );
        // Every frame again, well inside the interval: cleared after the hold, once.
        let mut cleared = 0;
        for id in 81..81 + u64::from(SHORT_HOLD) + 10 {
            if let Some(None) = pace.frame(id, id * interval, 2 * MS, true, "soft") {
                cleared += 1;
            }
        }
        assert_eq!(cleared, 1);
    }

    /// Every frame made: nothing is said.
    #[test]
    fn an_isp_that_keeps_up_says_nothing() {
        let mut pace = Pace::default();
        for id in 1..100u64 {
            assert!(pace.frame(id, id * 8 * MS, 3 * MS, true, "pisp").is_none());
        }
    }
}

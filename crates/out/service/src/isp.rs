//! A camera's processed stream: raw frames through an ISP, published beside the raw stream.
//!
//! With `isp` set in a camera's config (see [`crate::config::CameraConfig::isp`]), each attempt at
//! the camera runs one more thread, the camera's ISP thread. The capture thread offers it each raw
//! frame through a hand-off of [`HANDOFF`] frames; a frame that finds the hand-off full is skipped
//! for the ISP and counted (`isp_skipped`), and the raw socket still publishes it. The ISP thread
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
//! - **3A.** None: the service has no 3A loop. Exposure and gain stay the configured ones and the
//!   ISP runs neutral settings (unity white balance, identity colour matrix, the sRGB curve).
//!
//! With `isp = "auto"` the [`Switcher`] picks the ISP per frame: a frame the back end refuses is
//! made by the soft ISP, so its readers lose no frame to the refusal.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
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
use styx_soft::{Options, SoftIspStage};

use crate::config::{IspMode, IspPlan};
use crate::isp_switch::{Route, Switcher};
use crate::slot::Slot;

/// Raw frames waiting for the ISP thread at most.
pub(crate) const HANDOFF: usize = 2;
/// Raw buffers the ISP may hold at once: the hand-off and the frame it works on. The camera's
/// reserve is raised by this much while the ISP runs, so the camera always has its own.
pub(crate) const RAW_HOLD: usize = HANDOFF + 1;
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
}

impl Feed {
    /// Offers `raw` to the ISP. A full hand-off skips it for the ISP (counted).
    pub(crate) fn offer(&self, raw: &FrameLease<FrameMetadata>, slot: &Slot) {
        match self.frames.try_send(raw.share()) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => slot.isp_skipped(),
        }
    }
}

/// The hand-off between the capture thread and the ISP thread.
pub(crate) fn handoff() -> (Feed, Receiver<FrameLease<FrameMetadata>>) {
    let (frames, rx) = std::sync::mpsc::sync_channel(HANDOFF);
    (Feed { frames }, rx)
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
}

/// The ISP thread: runs until the capture thread drops its [`Feed`].
pub(crate) fn run(ctx: &Context<'_>, frames: &Receiver<FrameLease<FrameMetadata>>) {
    let (tx, sockets) = std::sync::mpsc::channel();
    ctx.slot.set_isp_route(Some(tx));
    ctx.slot.isp_starting(ctx.plan.mode);
    let mut stage: Option<Stage> = None;
    let mut down = false;
    loop {
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
                Options::default(),
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

        let mut stage = Self {
            mode: plan.mode,
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
        if !made {
            ctx.slot.isp_skipped();
        }
        self.show(ctx.slot);
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
        for out in &mut self.outs {
            let Some(index) = (out.be..out.be + out.soft).find(|&i| out.pool.is_free(i)) else {
                slot.isp_published(out.k, true, out.producer.stats().readers);
                continue;
            };
            let Ok(mut lease) = out.pool.acquire_at(index, out.entry(raw)) else {
                continue;
            };
            if let Err(e) = stage.process::<FrameMetadata, ()>(raw, &mut lease, None) {
                // The reason is shown; the frame is not published.
                drop(lease);
                self.reason.clear();
                self.reason.push_str("soft ISP: ");
                self.reason.push_str(&e.to_string());
                self.reason_at_ns = styx_socket::clock_ns();
                continue;
            }
            out.publish(lease, &self.blob[..len], slot);
            made = true;
        }
        if made {
            self.active = IspActive::Soft;
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

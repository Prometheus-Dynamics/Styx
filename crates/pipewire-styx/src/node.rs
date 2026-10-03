//! One Styx camera as a PipeWire `Video/Source` node (role `Camera`, so the camera portal and
//! browsers list it). The node offers what the planner can deliver; when a consumer picks a
//! format the capture starts, and it stops when the consumer goes.
//!
//! Buffers: the node allocates PipeWire's buffers itself (`ALLOC_BUFFERS`), one memfd each
//! ([`crate::pool`]), handed to consumers as memfds or, for formats negotiated with a DRM
//! modifier, as dma-bufs of the same pages. The camera captures into them where it can
//! (`CaptureBuffers`: the virtual camera, V4L2 drivers that import dma-bufs, when the plan passes
//! the camera's frames through unchanged): PipeWire buffer N is then camera buffer N, and the
//! frame in it is held until the consumer gives the buffer back, so the camera cannot refill it
//! while it is read. Otherwise each frame is copied into a free buffer, as before.

use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use pw::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use styx::capture_api::CaptureBuffers;
use styx::prelude::*;

use crate::capture::{Capture, Request, Source};
use crate::formats::{Offer, copy_packed, is_packed, packed_planes, packed_size};
use crate::params::{as_pods, buffers_pod, enum_format_pods, styx_format};
use crate::pool::{Slot, capture_buffers, dmabuf_available};

enum Msg {
    Frame(Box<FrameLease>),
    Failed(String),
    /// Start the capture if streaming (after buffers were (re)allocated).
    Start,
}

/// The negotiated format.
#[derive(Clone, Copy)]
struct Negotiated {
    request: Request,
    stride: u32,
    size: u32,
    /// Buffers are dma-bufs (the format carries a DRM modifier).
    dmabuf: bool,
}

/// A PipeWire buffer and the node's memory behind it.
struct Buffer {
    pw: *mut pw::sys::pw_buffer,
    slot: Slot,
    /// The frame captured into this buffer, held while a consumer has the buffer.
    frame: Option<FrameLease>,
    /// Dequeued by the node (not with a consumer).
    ours: bool,
}

#[derive(Default)]
struct Counts {
    frames: u64,
    in_place: u64,
    copied_bytes: u64,
}

struct Shared {
    name: String,
    source: Source,
    format: Option<Negotiated>,
    buffers: Vec<Buffer>,
    /// The buffers as given to the running capture.
    capture_buffers: Option<CaptureBuffers>,
    capture: Option<Capture>,
    latest: Option<FrameLease>,
    sender: pw::channel::Sender<Msg>,
    counts: Counts,
    dmabuf: bool,
}

impl Shared {
    fn start(&mut self) {
        let Some(format) = self.format else { return };
        if self.capture.is_some() || self.buffers.is_empty() {
            return;
        }
        let request = format.request;
        let media = Resolution::new(request.width, request.height)
            .map(|res| MediaFormat::new(request.fourcc, res, ColorSpace::Unknown));
        let planes = packed_planes(request.fourcc, request.width, request.height);
        // `STYX_PIPEWIRE_COPY=1`: copy every frame (to compare, or to rule the import out).
        let copy = std::env::var_os("STYX_PIPEWIRE_COPY").is_some_and(|v| v == "1");
        self.capture_buffers = match (media, planes) {
            (Some(media), Some(planes)) if !copy => {
                capture_buffers(media, planes, self.buffers.iter().map(|b| &b.slot))
                    .inspect_err(|err| {
                        eprintln!("{}: no buffers to capture into: {err}", self.name)
                    })
                    .ok()
            }
            _ => None,
        };
        eprintln!(
            "{}: streaming {} {}x{} @ {}/{} ({} {} buffers)",
            self.name,
            request.fourcc,
            request.width,
            request.height,
            request.rate.0,
            request.rate.1,
            self.buffers.len(),
            if format.dmabuf { "dma-buf" } else { "memfd" },
        );
        let frames = self.sender.clone();
        let failures = self.sender.clone();
        self.capture = Some(Capture::start(
            self.source.clone(),
            request,
            self.capture_buffers.clone(),
            move |frame| {
                let _ = frames.send(Msg::Frame(Box::new(frame)));
            },
            move |err| {
                let _ = failures.send(Msg::Failed(err));
            },
        ));
    }

    fn stop(&mut self) {
        if self.capture.take().is_some() {
            let c = std::mem::take(&mut self.counts);
            eprintln!(
                "{}: stopped after {} frames ({} captured in place, {} bytes copied)",
                self.name, c.frames, c.in_place, c.copied_bytes
            );
        }
        self.latest = None;
        // The camera stopped: let its buffers go (a restarted capture needs the device).
        for buffer in &mut self.buffers {
            buffer.frame = None;
        }
        self.capture_buffers = None;
    }

    /// Take back the buffers consumers returned; frames held in them go back to the camera.
    fn reclaim(&mut self, stream: &Stream) {
        loop {
            // SAFETY: returns a buffer of this stream or null.
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() {
                break;
            }
            // SAFETY: `raw` is one of this stream's buffers; `user_data` is our index.
            let index = unsafe { (*raw).user_data } as usize;
            if let Some(buffer) = self.buffers.get_mut(index) {
                buffer.ours = true;
                buffer.frame = None;
            }
        }
    }

    /// Send `frame` to the consumer: in the buffer it was captured into, or copied into a free
    /// one.
    fn send(&mut self, stream: &Stream, frame: FrameLease) {
        let Some(format) = self.format else { return };
        let request = format.request;
        let res = frame.meta().format.resolution;
        if (frame.meta().format.code, res.width.get(), res.height.get())
            != (request.fourcc, request.width, request.height)
        {
            return; // a camera service chose another size: not what was negotiated
        }
        let captured = self.capture_buffers.as_ref();
        let in_place = captured
            .and_then(|b| b.index_of(&frame))
            .filter(|_| is_packed(&frame));
        let index = match in_place {
            Some(index) if self.buffers.get(index).is_some_and(|b| b.ours) => index,
            // A consumer still has that buffer (cannot happen: the frame in it is held).
            Some(_) => return,
            // The free buffers are queued to the camera: do not write to them.
            None if captured.is_some_and(CaptureBuffers::in_use) => return,
            None => match self.buffers.iter().position(|b| b.ours) {
                Some(index) => index,
                None => return, // consumers hold every buffer: drop this frame
            },
        };
        if self.counts.frames == 0 {
            eprintln!(
                "{}: {}",
                self.name,
                match in_place {
                    Some(_) => "the camera captures into the PipeWire buffers (no copy)",
                    None => "copying frames into the PipeWire buffers",
                }
            );
        }
        let buffer = &mut self.buffers[index];
        let size = match in_place {
            Some(_) => format.size as usize,
            None => match copy_packed(&frame, buffer.slot.bytes_mut()) {
                Some(written) => written,
                None => return,
            },
        };
        // SAFETY: `pw` is a live buffer of this stream with one data block (checked when added),
        // dequeued by us; its chunk is ours to fill until it is queued.
        unsafe {
            let data = (*(*buffer.pw).buffer).datas;
            let chunk = (*data).chunk;
            (*chunk).offset = 0;
            (*chunk).size = size as u32;
            (*chunk).stride = format.stride as i32;
            (*chunk).flags = 0;
        }
        self.counts.frames += 1;
        match in_place {
            Some(_) => {
                self.counts.in_place += 1;
                buffer.frame = Some(frame);
            }
            None => self.counts.copied_bytes += size as u64,
        }
        buffer.ours = false;
        // SAFETY: `pw` was dequeued from this stream and is queued once.
        unsafe { stream.queue_raw_buffer(buffer.pw) };
    }

    /// PipeWire allocated buffer `raw`: back it with a new slot.
    fn add_buffer(&mut self, raw: *mut pw::sys::pw_buffer) {
        let Some(format) = self.format else { return };
        // SAFETY: `raw` is a buffer PipeWire just added to this stream; with `ALLOC_BUFFERS` its
        // data blocks are ours to fill, and `type_` holds the allowed types as a bit mask.
        unsafe {
            let spa_buffer = (*raw).buffer;
            if (*spa_buffer).n_datas < 1 {
                return;
            }
            let data = &mut *(*spa_buffer).datas;
            let memfd_ok = data.type_ & (1 << spa::sys::SPA_DATA_MemFd) != 0;
            let dmabuf = format.dmabuf || !memfd_ok;
            let slot = match Slot::new(format.size as usize, self.dmabuf || dmabuf) {
                Ok(slot) => slot,
                Err(err) => {
                    eprintln!("{}: cannot allocate a buffer: {err}", self.name);
                    return;
                }
            };
            if dmabuf {
                let Some(fd) = &slot.dmabuf else {
                    eprintln!("{}: consumer wants dma-bufs; no /dev/udmabuf", self.name);
                    return;
                };
                data.type_ = spa::sys::SPA_DATA_DmaBuf;
                data.fd = fd.as_raw_fd() as i64;
                data.data = std::ptr::null_mut();
            } else {
                data.type_ = spa::sys::SPA_DATA_MemFd;
                data.fd = slot.memfd.as_raw_fd() as i64;
                data.data = slot.ptr().cast();
            }
            data.flags = spa::sys::SPA_DATA_FLAG_READWRITE;
            data.mapoffset = 0;
            data.maxsize = slot.len as u32;
            (*raw).user_data = self.buffers.len() as *mut std::ffi::c_void;
            self.buffers.push(Buffer {
                pw: raw,
                slot,
                frame: None,
                ours: false,
            });
        }
    }

    fn remove_buffer(&mut self, raw: *mut pw::sys::pw_buffer) {
        // The camera may be filling it.
        self.stop();
        self.buffers.retain(|b| b.pw != raw);
        for (index, buffer) in self.buffers.iter().enumerate() {
            // SAFETY: the remaining buffers are live buffers of this stream.
            unsafe { (*buffer.pw).user_data = index as *mut std::ffi::c_void };
        }
    }
}

/// A published camera; dropping it removes the node.
pub struct Node<'l> {
    _stream: StreamRc,
    _listener: StreamListener<()>,
    _receiver: pw::channel::AttachedReceiver<'l, Msg>,
}

pub fn publish<'l>(
    core: &pw::core::CoreRc,
    loop_: &'l pw::loop_::Loop,
    name: &str,
    description: &str,
    source: Source,
    offers: &[Offer],
) -> Result<Node<'l>, pw::Error> {
    let node_name = format!("styx.{}", sanitize(name));
    let stream = StreamRc::new(
        core.clone(),
        &node_name,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => node_name.as_str(),
            *pw::keys::NODE_DESCRIPTION => description,
        },
    )?;
    let (sender, receiver) = pw::channel::channel::<Msg>();
    let dmabuf = dmabuf_available();
    let shared = Rc::new(RefCell::new(Shared {
        name: node_name.clone(),
        source,
        format: None,
        buffers: Vec::new(),
        capture_buffers: None,
        capture: None,
        latest: None,
        sender,
        counts: Counts::default(),
        dmabuf,
    }));

    let on_param = shared.clone();
    let on_state = shared.clone();
    let on_process = shared.clone();
    let on_add = shared.clone();
    let on_remove = shared.clone();
    let listener = stream
        .add_local_listener::<()>()
        .param_changed(move |stream, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let Some(fourcc) = styx_format(info.format()) else {
                return;
            };
            let size = info.size();
            let rate = info.framerate();
            let Some((stride, bytes)) = packed_size(fourcc, size.width, size.height) else {
                return;
            };
            let dmabuf = info.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            let mut shared = on_param.borrow_mut();
            shared.stop();
            shared.format = Some(Negotiated {
                request: Request {
                    fourcc,
                    width: size.width,
                    height: size.height,
                    rate: (rate.num, rate.denom),
                },
                stride,
                size: bytes,
                dmabuf,
            });
            let _ = shared.sender.send(Msg::Start);
            let name = shared.name.clone();
            // PipeWire may (re)allocate buffers from within: release `shared` first.
            drop(shared);
            let buffers = [buffers_pod(bytes, stride, dmabuf)];
            if let Err(err) = stream.update_params(&mut as_pods(&buffers)) {
                eprintln!("{name}: cannot set buffers: {err}");
            }
        })
        .add_buffer(move |stream, _, raw| {
            let mut shared = on_add.borrow_mut();
            shared.add_buffer(raw);
            if matches!(stream.state(), StreamState::Streaming) {
                let _ = shared.sender.send(Msg::Start);
            }
        })
        .remove_buffer(move |_, _, raw| on_remove.borrow_mut().remove_buffer(raw))
        .state_changed(move |_, _, _, new| match new {
            StreamState::Streaming => on_state.borrow_mut().start(),
            StreamState::Error(err) => {
                let mut shared = on_state.borrow_mut();
                eprintln!("{}: stream error: {err}", shared.name);
                shared.stop();
            }
            _ => on_state.borrow_mut().stop(),
        })
        .process(move |stream, _| {
            let mut shared = on_process.borrow_mut();
            shared.reclaim(stream);
            if let Some(frame) = shared.latest.take() {
                shared.send(stream, frame);
            }
        })
        .register()?;

    let weak = stream.downgrade();
    let on_msg = shared.clone();
    let receiver = receiver.attach(loop_, move |msg| match msg {
        Msg::Frame(frame) => {
            let mut shared = on_msg.borrow_mut();
            if shared.capture.is_none() {
                // Sent before the capture stopped: holding it would keep the camera open.
                return;
            }
            shared.latest = Some(*frame);
            drop(shared);
            if let Some(stream) = weak.upgrade() {
                let _ = stream.trigger_process();
            }
        }
        Msg::Failed(err) => {
            let mut shared = on_msg.borrow_mut();
            eprintln!("{}: capture failed: {err}", shared.name);
            shared.stop();
        }
        Msg::Start => {
            if weak
                .upgrade()
                .is_some_and(|s| matches!(s.state(), StreamState::Streaming))
            {
                on_msg.borrow_mut().start();
            }
        }
    });

    let formats = enum_format_pods(offers, dmabuf);
    stream.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
        &mut as_pods(&formats),
    )?;
    eprintln!(
        "{node_name}: published ({} formats{})",
        formats.len(),
        if dmabuf { ", also as dma-bufs" } else { "" }
    );
    Ok(Node {
        _stream: stream,
        _listener: listener,
        _receiver: receiver,
    })
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

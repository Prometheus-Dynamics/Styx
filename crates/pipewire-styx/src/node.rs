//! One Styx camera as a PipeWire `Video/Source` node (role `Camera`, so the camera portal and
//! browsers list it). The node offers what the planner can deliver; when a consumer picks a
//! format the capture starts, and it stops when the consumer goes.

use std::cell::RefCell;
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use styx::prelude::FrameLease;

use crate::capture::{Capture, Request, Source};
use crate::formats::{Offer, copy_packed, packed_size};
use crate::params::{as_pods, buffers_pod, enum_format_pods, styx_format};

enum Msg {
    Frame(FrameLease),
    Failed(String),
}

struct Shared {
    name: String,
    source: Source,
    request: Option<Request>,
    capture: Option<Capture>,
    latest: Option<FrameLease>,
    sender: pw::channel::Sender<Msg>,
    frames: u64,
}

impl Shared {
    fn start(&mut self) {
        let Some(request) = self.request else { return };
        if self.capture.is_some() {
            return;
        }
        eprintln!(
            "{}: streaming {} {}x{} @ {}/{}",
            self.name,
            request.fourcc,
            request.width,
            request.height,
            request.rate.0,
            request.rate.1
        );
        let frames = self.sender.clone();
        let failures = self.sender.clone();
        self.capture = Some(Capture::start(
            self.source.clone(),
            request,
            move |frame| {
                let _ = frames.send(Msg::Frame(frame));
            },
            move |err| {
                let _ = failures.send(Msg::Failed(err));
            },
        ));
    }

    fn stop(&mut self) {
        if self.capture.take().is_some() {
            eprintln!("{}: stopped after {} frames", self.name, self.frames);
        }
        self.latest = None;
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
    let shared = Rc::new(RefCell::new(Shared {
        name: node_name.clone(),
        source,
        request: None,
        capture: None,
        latest: None,
        sender,
        frames: 0,
    }));

    let on_param = shared.clone();
    let on_state = shared.clone();
    let on_process = shared.clone();
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
            let mut shared = on_param.borrow_mut();
            shared.stop();
            shared.request = Some(Request {
                fourcc,
                width: size.width,
                height: size.height,
                rate: (rate.num, rate.denom),
            });
            let buffers = [buffers_pod(bytes, stride)];
            if let Err(err) = stream.update_params(&mut as_pods(&buffers)) {
                eprintln!("{}: cannot set buffers: {err}", shared.name);
            }
            if matches!(stream.state(), StreamState::Streaming) {
                shared.start();
            }
        })
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
            let (frame, request) = {
                let mut shared = on_process.borrow_mut();
                (shared.latest.take(), shared.request)
            };
            let (Some(frame), Some(request)) = (frame, request) else {
                return;
            };
            let res = frame.meta().format.resolution;
            if (frame.meta().format.code, res.width.get(), res.height.get())
                != (request.fourcc, request.width, request.height)
            {
                return; // a camera service chose another size: not what was negotiated
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return; // consumer holds every buffer: drop this frame
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let Some((stride, _)) =
                packed_size(frame.meta().format.code, res.width.get(), res.height.get())
            else {
                return;
            };
            let written = data.data().and_then(|dst| copy_packed(&frame, dst));
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = written.unwrap_or(0) as u32;
            on_process.borrow_mut().frames += 1;
        })
        .register()?;

    let weak = stream.downgrade();
    let on_msg = shared.clone();
    let receiver = receiver.attach(loop_, move |msg| match msg {
        Msg::Frame(frame) => {
            on_msg.borrow_mut().latest = Some(frame);
            if let Some(stream) = weak.upgrade() {
                let _ = stream.trigger_process();
            }
        }
        Msg::Failed(err) => {
            let mut shared = on_msg.borrow_mut();
            eprintln!("{}: capture failed: {err}", shared.name);
            shared.stop();
        }
    });

    let formats = enum_format_pods(offers);
    stream.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
        &mut as_pods(&formats),
    )?;
    eprintln!("{node_name}: published ({} formats)", formats.len());
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

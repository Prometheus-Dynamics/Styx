# PipeWire reliable transport (`node.reliable`): two faults seen from an output stream

Status: observed, not reported upstream. Whether to report them is the user's decision.

Seen on libpipewire 1.6.11 (the distribution's package; the code read is the 1.6.8 tag, where the two
functions below are the same), with pipewire-rs 0.10.1. `node.reliable` is new in PipeWire 1.6.0.

## Why Styx looked at it

`styx-pipewire` offers a producer's buffers to PipeWire without a copy, so it must know when a
consumer is done with a buffer before the producer may write it again. On an ordinary link
PipeWire gives a sent buffer back to an output stream one graph cycle later whether or not the
consumer took it. With `node.reliable = true` the output port hands a buffer over only when the
consumer's io area is free, and takes back the buffer id the consumer returned, which is the
signal wanted. Styx does not use it, because of the two faults below, and uses the rule in
`crates/bridge/pipewire/src/hold.rs` instead.

## Fault 1: `pw_stream_dequeue_buffer` reads an unset io area

**What happens.** An output stream with `node.reliable = true` that asks for a buffer before a
consumer is linked crashes with SIGSEGV inside `pw_stream_dequeue_buffer`.

**Where.** `src/pipewire/stream.c`, `pw_stream_dequeue_buffer`:

```c
if (impl->direction == SPA_DIRECTION_OUTPUT && stream->node->reliable) {
        struct spa_io_buffers *io = impl->io;

        if (!queue_is_empty(impl, &impl->queued) || io->status == SPA_STATUS_HAVE_DATA) {
```

`impl->io` is set by `impl_port_set_io` when the port gets its buffers io area and is `NULL`
before the stream is linked and again after it is unlinked. `io->status` is read without a check.

**Steps.** Build the program at the end of this note (`cargo new`, `pipewire = "0.10.1"`), with a
PipeWire daemon running and nothing linked to the stream:

- `pw-reliable-repro plain early`: runs 100 ticks and exits 0.
- `pw-reliable-repro reliable early`: SIGSEGV (exit 139) on the first tick. Backtrace:

```
#0  pw_stream_dequeue_buffer () from /lib64/libpipewire-0.3.so.0
#1  pipewire::stream::Stream::dequeue_raw_buffer
#2  pw_reliable_repro::main::{closure#0}
```

**What Styx does.** The node asks for no buffer while the buffers io area is unset (it follows
`io_changed`), so a `node.reliable` set from outside cannot crash it.

## Fault 2: once every buffer is out, none comes back

**What happens.** With `node.reliable = true`, an output stream that has sent every one of its
buffers gets none back, although the consumer has returned them. The stream stops for good.

**Where, as read from the source (not confirmed by a trace).** `src/pipewire/impl-port.c`,
`tee_process_reliable` picks up the buffer id a consumer returned (`io->buffer_id =
mio->buffer_id`) only inside `if (io->status == SPA_STATUS_HAVE_DATA)`, that is, only while it
hands a new buffer over. With nothing left to send, the returned ids stay in the consumer's io
area. And `pw_stream_dequeue_buffer` returns `EAGAIN` while anything is queued, so the stream
cannot keep one buffer back to send later either: a stream whose consumer holds buffers a little
longer than a cycle runs out.

**Steps.** In a Styx checkout at a commit that has `crates/bridge/pipewire/tests/consumer.rs`
(four buffers, GStreamer `pipewiresrc` as the consumer), with a PipeWire daemon and
`gst-launch-1.0`:

- `cargo test -p styx-pipewire --features pipewire --test consumer a_small`: passes, 40 frames.
- `PIPEWIRE_PROPS='{ node.reliable = true }' cargo test -p styx-pipewire --features pipewire --test consumer a_small`:
  fails after 30 s with `the consumer got no 40 frames` and the node's counts `frames: 5,
  released: 1`: five frames went out, one buffer came back, then nothing.

`PIPEWIRE_PROPS` is PipeWire's own way to add properties to every stream a process makes, so it
also shows what a user's environment can do to the node. A standalone program for this fault was
not finished: the one below was extended to send buffers to `pipewiresrc`, but the link never
negotiated, so it is given here for fault 1 only.

## The program for fault 1

```rust
//! PipeWire 1.6: `pw_stream_dequeue_buffer` on an output stream with `node.reliable = true`
//! reads the port's buffers io area, which is unset until the stream is linked.
use std::io::Cursor;
use std::time::Duration;

use pipewire::spa::param::ParamType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value, object, property};
use pipewire::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pipewire::stream::{StreamFlags, StreamRc};
use pipewire::{context::ContextRc, keys, main_loop::MainLoopRc, properties::properties};

fn main() {
    let reliable = std::env::args().nth(1).as_deref() != Some("plain");
    pipewire::init();
    let mainloop = MainLoopRc::new(None).unwrap();
    let context = ContextRc::new(&mainloop, None).unwrap();
    let core = context.connect_rc(None).unwrap();
    let stream = StreamRc::new(
        core,
        "reliable-repro",
        properties! {
            *keys::MEDIA_TYPE => "Video",
            *keys::MEDIA_CATEGORY => "Source",
            *keys::MEDIA_CLASS => "Video/Source",
            *keys::NODE_NAME => "reliable-repro",
            "node.reliable" => if reliable { "true" } else { "false" },
        },
    )
    .unwrap();
    let format = object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::VideoFormat, Id, VideoFormat::NV12),
        property!(FormatProperties::VideoSize, Rectangle, Rectangle { width: 64, height: 48 }),
        property!(FormatProperties::VideoFramerate, Fraction, Fraction { num: 30, denom: 1 }),
    );
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(format))
        .unwrap()
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).unwrap()];
    stream
        .connect(Direction::Output, None, StreamFlags::empty(), &mut params)
        .unwrap();
    // `early`: ask for a buffer before any consumer is linked (fault 1).
    // otherwise: only while streaming; send every buffer PipeWire gives back (fault 2).
    let early = std::env::args().nth(2).as_deref() == Some("early");
    let ticks = std::cell::Cell::new(0);
    let sent = std::cell::Cell::new(0);
    let quit = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        ticks.set(ticks.get() + 1);
        let streaming = matches!(stream.state(), pipewire::stream::StreamState::Streaming);
        if early || streaming {
            // SAFETY: the loop's thread; the buffer is queued straight back.
            let buffer = unsafe { stream.dequeue_raw_buffer() };
            if !buffer.is_null() {
                sent.set(sent.get() + 1);
                unsafe { stream.queue_raw_buffer(buffer) };
            }
        }
        if ticks.get() % 10 == 0 {
            println!("tick {}: state {:?}, buffers sent so far {}", ticks.get(), stream.state(), sent.get());
        }
        if ticks.get() == 100 {
            quit.quit();
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100)));
    mainloop.run();
    println!("ran 100 ticks");
}
```

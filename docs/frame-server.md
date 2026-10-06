# Frames for Other Processes

`styx::ipc` shares camera frames with other processes on the same machine (Linux), without
copying them.

- **`CameraService`** owns a camera. Each client process asks for the frames it needs, and the
  service plans one shared capture for all of them.
- **`FrameServer`** publishes frames your own code produces to whoever connects.
- **`FrameSocket`** (feature `frame-socket`) serves the latest frame, leased, in the
  `styx-frame-lease-v1` format HeliOS uses (see [Frame socket](#frame-socket-styx-frame-lease-v1)).

The first two deliver to a `FrameClient`, which receives ordinary `FrameLease`s.

## Camera service

```rust
use styx::ipc::CameraService;

// The process that owns the camera.
let service = CameraService::new(device).serve("/run/styx/front.sock")?;
```

```rust
use styx::ipc::FrameClient;

// A detector process.
let frames = FrameClient::request(
    "/run/styx/front.sock",
    &Frames::gray().size(320, 180),
)?;
print!("{}", frames.plan().unwrap_or_default()); // what the service planned for it
let delivered = frames.delivered().unwrap(); // format, size, fps, pyramid, unmet
while let RecvOutcome::Data(frame) = frames.recv(Duration::from_secs(1)) {
    // 320x180 (or 320x200: the camera's aspect ratio) Y8, in the camera's or a memfd's memory
}
```

- **Planning:** the service runs `plan_many` over every connected client's requirements. Clients
  get everything shared plans do: hardware scaling on both Raspberry Pi ISP outputs, decoding
  once for clients with the same needs, and each client's own ROI (`FrameClient::set_roi`).
- **Joining:** a client whose frames the running capture can already give is attached without
  disturbing the others. One that needs another mode or ISP setup restarts the capture once, for
  everyone; the other clients stay connected and see a short gap.
- **Refusing:** a request the camera cannot serve next to the others fails with
  `IpcError::Rejected` and the planner's reasons; the other clients are not affected.
- **What arrives:** `FrameClient::delivered()` (from the service's answer, before the first
  frame) gives the format, size, frame rate and pyramid of the frames, and what of the request
  they do not meet (see [frame-planning.md](frame-planning.md#what-arrives-and-strict-requests)).
  A `.strict()` request is refused instead of being served less.
- **Timeouts:** opening waits up to 10 s for the connection and the answer together;
  `FrameClient::options(path).timeout(d).request(&frames)` sets another, and `request_async`
  (feature `async`) awaits it without blocking a thread, giving up when the future is dropped.
- **Idle:** a client gets frames only as fast as it drops them, so a camera nobody reads from
  idles. By default the service pauses it after 2 s (libcamera stays configured and wakes in
  ~0.1 s; V4L2 releases). `stop_when_idle` releases instead, `keep_streaming` never stops.
- **No copies:** frames the plan decodes go straight into memfds (`FramePlan::exportable`), and
  camera buffers are passed as dma-bufs. `CameraServiceStats::copied` counts exceptions.

`examples/05_apps/camera_service.rs` runs a service and clients from the command line.

### Several cameras

`CameraService::all_cameras()` serves every camera, probing when a client asks, so cameras
plugged in later appear; `CameraService::with_cameras(devices)` serves a fixed set.
`FrameClient::cameras(path)` lists them (name, identity keys, whether in use), and
`FrameClient::request_camera(path, "ov9782", &requirements)` names one by its name, part of it,
or an identity key; `FrameClient::request` takes the first. Each camera has its own shared
capture and clients. A camera that goes away while in use is reconnected by the capture
supervisor when it comes back (see [reconnect.md](reconnect.md)).

### Encoded streams

Clients may ask for H.264, H.265 or MJPEG from a camera that does not produce them: the planner
adds an encoder (hardware first: VA-API, V4L2 mem2mem; else libx264/libx265 at low latency),
after a decoder for MJPEG cameras. Clients asking for the same stream share one encoder. Each
starts at a keyframe carrying the stream headers: a client joining a running stream, or one that
fell behind and lost packets, gets a keyframe made for it (`FrameMeta::delta` marks packets that
need the ones before them). `Frames::request_keyframe` asks for one in-process.

### Protecting the service

Requests come from other processes. The service:

- checks each request before planning it: sizes up to 16384, queue depth 1–8, at most 64 decode
  threads, 4 pyramid levels, power-of-two alignment up to 4096, short override names;
- serves at most `max_clients` clients (default 16) and closes connections beyond three per
  client (frames, controls, control events) and 8 more, even before they send a request, and
  drops clients that send nothing within 5 s;
- can check who connects: `authorize(|peer| peer.uid == 1000)` gets the kernel's process, user
  and group IDs for the connection; `socket_mode(0o660)` sets who may open the socket.

Message decoding is fuzzed (`fuzz/`, target `ipc_messages`, control messages included): 46
million inputs found nothing in 5 minutes.

### Camera controls

A client sets, reads and lists the shared camera's controls over the service's socket:

```rust
let applied = frames.set_exposure_us(8000)?;       // or set_gain, set_ae, set_ev, set_fps,
                                                   // set_awb, set_colour_temperature,
                                                   // set_colour_gains, set_af_mode, trigger_af,
                                                   // set_lens_position
println!("{:?} in effect from frame {:?}", applied.value, applied.frame);
frames.set_control(ControlId(0x0098_0900), ControlValue::Int(40))?; // a backend's own control
let gain = frames.get_control(StandardControl::Gain)?;
for c in frames.controls()? {                       // for a settings screen
    println!("{} {:?}..{:?} now {:?} {:?} writable {}",
        c.meta.name, c.meta.min, c.meta.max, c.current, c.standard, c.writable);
}
let events = frames.control_events()?;              // changes by any client
while let RecvOutcome::Data(e) = events.recv(Duration::from_secs(1)) { /* e.id, e.value, e.by */ }
```

- **Which control:** a backend's id (as `controls()` lists them: native, libcamera, V4L2, UVC,
  virtual), or a `StandardControl` in one set of units whatever the backend: exposure (µs),
  gain (ratio), AE, EV (stops), frame rate (fps), AWB, colour temperature (K), red and blue
  gains, AF mode, AF trigger, lens position (dioptres). The service finds the backend's control
  by its snake-case name (native and virtual cameras: `exposure_time_us`, ...), its libcamera
  name (`ExposureTime`, `AnalogueGain`, `AeEnable`, ...) or its V4L2/UVC id (exposure in
  100 µs and EV in thousandths converted; gains and lens positions in the device's units).
- **The answer:** `AppliedControl`: the value in effect (read back where the backend applies
  at once: V4L2, UVC, virtual; else the value applied, which later frames carry in their
  metadata: `NativeFrameMeta` exposure and gains), `clamped` when the value was outside the
  control's range or between its steps (numbers of any type are taken for the control's type),
  `frame`: on a native camera's raw modes the sensor sequence of the first frame using it,
  `deferred` while the camera is stopped (applied when it starts), `restarted` for a frame
  rate. Refused: `IpcError::ControlRefused(ControlRefusal::...)`: `Unsupported` (no such
  control), `ReadOnly`, `Invalid` (wrong type, not a menu entry), `NotPermitted` (the policy),
  `Failed` (the camera refused it).
- **Frame rate:** through the camera's control where it has one that works while streaming
  (a native camera's raw modes); otherwise the service plans every client's frames at that
  rate and restarts the capture once for all of them (`AppliedControl::restarted`; listed and
  answered as `SERVICE_FRAME_RATE`), or, with `ControlPolicy::no_restart()`, refuses with that
  reason. A rate the camera cannot give is refused with the planner's reason and the old
  capture continues.
- **Kept:** controls clients set are applied again whenever the capture restarts (a client
  joining with another setup, a frame rate); an AF trigger is not repeated.
- **Who may:** `CameraService::control_policy(ControlPolicy)`. By default any client may
  change any writable control; conflicting writes: the last one wins, and every subscribed
  client is told. `ControlPolicy::owner_only()`: only the camera's owner, its longest-connected
  client (the next one when it leaves). `read_only()`: nobody. `read_only_control(id or
  StandardControl)`: that one stays as it is. `allow(|caller, id| ...)`: an allow-list given
  the caller's process credentials, client id and ownership. Reading and listing are always
  allowed; `ControlDescriptor::writable` says what this client may change.
- **Events:** `control_events()` opens a connection that gets every accepted change on the
  camera (`ControlEvent`: id, standard control, value in effect, frame, and `by`, the client
  that made it, as `FrameClient::client_id` names clients); readable (`AsFd`) when one waits.
- **Wire:** requests go on a connection of their own (opened on first use), never between
  frames. New message kinds (Control, ControlReply, ControlEvent) and a trailer on the accept
  message (the client's id and a token proving it on control requests); the protocol version
  stays 8: older clients never send the new requests and never get the new messages (events go
  only to connections that subscribed), and read the accept message as before. A control list
  travels in a memfd. A service that predates controls does not answer: the request times out.

`camera_service controls [--camera NAME] [NAME VALUE]` (example) lists a camera's controls or
sets one. Tests: `crates/styx/tests/service_controls.rs` (virtual cameras with controls:
`capture_api::make_virtual_device_with_controls`).

### Control clients (no frames)

A settings screen, or a process that only steers exposure, needs no frames: a `ControlClient`
has the same control methods as a `FrameClient` (`set_control`, `get_control`, `controls`, the
typed setters) and follows changes, but makes no frame request:

```rust
use styx::ipc::ControlClient;

let controls = ControlClient::connect_camera(path, "front")?;  // or ControlClient::connect(path)
controls.set_exposure_us(8000)?;
let gain = controls.get_control(StandardControl::Gain)?;
while let RecvOutcome::Data(e) = controls.try_event() { /* e.id, e.value, e.by */ }

// Without blocking, before the service is up, and across its restarts:
let controls = ControlClient::options(path).camera("front").reconnecting().controls_nonblocking()?;
// ... poll(controls.as_raw_fd()) in the engine's loop, then drain try_event() ...
controls.ready().await?;                                       // or await it, on any executor
controls.set_control_async(StandardControl::ExposureUs, ControlValue::Uint(8000)).await?;
```

- **Not a frame client:** it never joins the camera's frame plan, so connecting or leaving never
  changes the plan, starts, restarts or stops the capture, or keeps it from idling, and it
  holds no buffers. It is not counted in `CameraServiceStats::clients`, `CameraInfo::in_use` or
  the per-client metrics, nor against `max_clients` (its connections count toward the
  service's connection limit). A frame rate the camera cannot change while streaming still
  restarts the capture for every client when one is set, as the policy allows.
- **Policy:** `ControlPolicy` applies to it like any client: `ControlCaller::client` is `None`
  and it is never the owner (under `owner_only()` it can read and list, not change).
- **While nothing streams:** a control set is remembered and applied when a frame client starts
  the capture (`AppliedControl::deferred`); reads return the value set, else the default.
- **Events:** the client's own connection is an event subscription: its descriptor (`AsFd`) is
  readable when `try_event()` has a change or news (connected, closed, the next attempt due);
  `recv_event(wait)`, `next_event().await`, `events()` (a `Stream`). Changes it makes are
  announced with `by: None`. `try_client_event()` adds connection changes, in order (see
  "Connection changes"): re-apply settings on `Connected`.
- **Requests:** `set_control`, `get_control`, `controls` wait for the service's answer (up to
  the timeout; an error at once when the service is not there); `set_control_async`,
  `get_control_async`, `controls_async` await it on any executor (styx-graph's reactor), never
  blocking a thread. `FrameClient` has the async forms too. They use a connection of their own,
  opened on first use and again after the service restarted.
- **Wire:** nothing new: a control request without a client token names a camera (or none: the
  first), as before; the service looks the camera up once per connection.

`service_controls` (example; `--demo` for a virtual camera) sets exposure and prints every
change; `crates/styx/tests/control_client.rs` tests control clients (no plan change, no restart,
no capture started, no frames sent, the policy) and non-blocking connections.

### Without a thread per client

A `FrameClient` need not have a thread blocked in `recv`:

- **Pollable:** it is a file descriptor (`AsFd`, `AsRawFd`; an epoll set of its own, the same
  across reconnections) that is readable when `try_next()` has a frame or news (closed; a
  reconnecting client's next attempt or the service's answer). `try_next()` never blocks: a
  frame, `Empty`, or `Closed` (and the descriptor stays readable after that, so a poll loop
  learns it). A reconnecting client reconnects inside `try_next` without blocking either.
- **Connecting:** `ClientOptions::request_nonblocking` returns at once, before the service
  answers (see "Connecting without blocking"); `client.ready().await` waits for it.
- **Async on any executor:** `client.next().await`, `client.stream()` (a
  `futures_core::Stream`) and `client.poll_next(cx)` for hand-written futures wake through
  styx-graph's reactor (one thread per process, started on first use): tokio, smol, or
  `styx_graph::rt::block_on` with no runtime at all. One task polls a client at a time.
  `recv_async` (feature `async`) stays for Tokio.
- **The same cost:** one allocation per frame (its release record), no copies, nothing on the
  reactor's thread (`tests/zero_alloc.rs`).

Many cameras on one thread (for HeliOS: N cameras into Daedalus host inputs):

```rust
let clients: Vec<FrameClient> = ["front", "left", "right"].iter()
    // Non-blocking: a camera (or the service) that is not there yet does not hold this up.
    .map(|cam| FrameClient::options(path).camera(*cam).reconnecting()
        .request_nonblocking(&Frames::nv12()))
    .collect::<Result<_, _>>()?;

// One poll(2) (or an epoll set, or the GUI loop's descriptor watch) over every client:
let mut fds: Vec<libc::pollfd> = clients.iter()
    .map(|c| libc::pollfd { fd: c.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
loop {
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
    for (i, p) in fds.iter().enumerate().filter(|(_, p)| p.revents != 0) {
        while let RecvOutcome::Data(frame) = clients[i].try_next() {
            hosts[i].push_payload("frame", frame_payload(frame)); // or any consumer
        }
    }
}

// Or one future over every client, on any executor:
poll_fn(|cx| {
    for (i, c) in clients.iter().enumerate() {
        while let Poll::Ready(RecvOutcome::Data(frame)) = c.poll_next(cx) {
            hosts[i].push_payload("frame", frame_payload(frame));
        }
    }
    Poll::<()>::Pending
}).await;
```

`examples/05_apps/camera_service_async.rs` runs both (`--demo`: three virtual cameras in the
same process); `crates/styx/tests/frame_client_async.rs` tests them (three clients of two
cameras on one thread, a service going away, a reconnecting client coming back).

### Reconnecting clients

`FrameClient::request(...)?.reconnecting()` survives service restarts: when the connection
drops, receives return `Empty` instead of `Closed` while the client reconnects (backing off from
100 ms to 2 s) and asks for the same frames again, with its latest region of interest.
`FrameClient::reconnects()` counts the reconnections.

### Connecting without blocking

`FrameClient::request` (and `ControlClient::connect`) wait for the service's answer, up to the
timeout: a graph thread opening a camera that is not there would wait. Made with
`request_nonblocking` (or `controls_nonblocking`), a client comes back at once and connects in
the background:

```rust
let client = FrameClient::options(path)
    .camera("left")
    .reconnecting()                     // keep trying until the service and camera are there
    .request_nonblocking(&Frames::nv12())?;  // never waits; fails only without descriptors
// poll loop: the descriptor is readable when the answer comes or the next attempt is due
match client.try_next() {
    RecvOutcome::Data(frame) => {}      // connected, frames flowing
    RecvOutcome::Empty => {}            // not ready (yet): not an error
    RecvOutcome::Closed => {}           // gave up (not reconnecting): client.last_error()
}
// or async, on any executor: the awaitable form of FrameClient::request
client.ready().await?;
```

- **Steps, not threads:** each attempt is a non-blocking connect and the request; the answer is
  taken when it arrives. `try_next`, `poll_next`/`next().await`, `ready().await` and `recv(wait)`
  drive it (`recv` never waits longer than `wait`, connecting or not). The client's descriptor
  wakes for the answer and for the next attempt, so a poll loop never spins or blocks.
- **Until connected:** `try_next` returns `Empty`, `is_connected()` is false, `delivered()`
  and `plan()` are `None`, and `last_error()` says why the last attempt failed (no service,
  timed out, `Rejected`: no such camera, or it cannot serve the request). Control requests work
  meanwhile, as a control client's (no token yet).
- **Giving up:** a reconnecting client never does (backing off from 100 ms to 2 s; its first
  connection is not counted in `reconnects()`). One that does not reconnect gives up when the
  service refuses the request or has not accepted it within `ClientOptions::timeout`: then
  `try_next` returns `Closed` (the descriptor stays readable) and `ready()` the error.
- **The blocking API is unchanged:** `FrameClient::request`, `request_camera`,
  `ClientOptions::request` and `request_async` (Tokio) still wait for the answer and return
  its error.

### Connection changes

A client that reconnects says so: `try_client_event()` returns what `try_next()` (frames) or
`try_event()` (control changes) return, plus each change of the connection, in order, as a
`ClientEvent`:

```rust
use styx::ipc::{ClientEvent, ControlClient};

let controls = ControlClient::options(path).camera("front").reconnecting().controls_nonblocking()?;
// poll(controls.as_raw_fd()) in the engine's loop, then:
loop {
    match controls.try_client_event() {
        RecvOutcome::Data(ClientEvent::Connected { reconnects }) => {
            // The first connection (reconnects == 0) and every reconnection: the service may
            // have restarted with defaults, so apply the persisted settings again.
            controls.set_exposure_us(settings.exposure_us)?;
        }
        RecvOutcome::Data(ClientEvent::Disconnected { error }) => { /* show "camera offline" */ }
        RecvOutcome::Data(ClientEvent::Data(event)) => { /* a control change */ }
        RecvOutcome::Empty => break,           // nothing more: wait for the descriptor again
        RecvOutcome::Closed => break,          // gave up (not reconnecting)
    }
}
// Or awaited, on any executor (futures' StreamExt::next):
let mut events = controls.client_events();
while let Some(event) = events.next().await { /* the same */ }
```

`FrameClient` has the same: `try_client_event()`, `poll_client_event(cx)`,
`next_client_event().await` and `client_events()` (a `Stream`), with `ClientEvent::Data(frame)`.

- **Once each, in order:** `Connected { reconnects }` for the first connection and each
  reconnection (`reconnects` as `reconnects()` counts them: 0 the first time),
  `Disconnected { error }` when the connection is lost (a `ConnectionReset` I/O error when the
  service closed it). What arrived on a connection comes before its `Disconnected`; nothing from
  a new connection comes before its `Connected`.
- **Wakes:** the client's descriptor (`AsFd`) becomes readable for each change, and a task in
  `poll_client_event`/`next_client_event`/`client_events` is woken, so a poll loop never has
  to check `is_connected()`. Once read, a change does not wake it again.
- **The first connection:** reported by every client, also when a blocking `request`/`connect`
  already returned connected: `Connected { reconnects: 0 }` is then its first event.
- **Giving up:** a client that does not reconnect reports `Disconnected` once (the connection
  lost, or, never connected, why it gave up), then `Closed`, and the stream ends.
- **Between reads:** changes are queued, each kept until read, so several reconnections between
  two reads are all reported, in order. At most 16 wait: beyond, the oldest pair is dropped (a
  client that reads its events never gets near, reconnections being at least 100 ms apart), so
  what is read still alternates and ends with the state now.
- **Compatibility:** `try_next`, `recv`, `poll_next`, `next`, `stream`, `try_event`,
  `recv_event`, `poll_event`, `next_event` and `events` are unchanged: they return no connection
  changes (and leave them unread). Use `try_client_event` and its forms instead of them, not next
  to them: reading with both splits what arrives between them. Moving from `try_event` is a
  `match` arm: `RecvOutcome::Data(event)` becomes `RecvOutcome::Data(ClientEvent::Data(event))`.
- **Requests after a restart:** control requests reconnect on their own; one made on
  `Connected` goes to the new service.

## Frame server

```rust
use styx::ipc::FrameServer;

let server = FrameServer::bind("/run/styx/front.sock")?;
let mut frames = Frames::gray().open(&device)?;
for frame in &mut frames {
    server.publish(&frame)?; // never blocks on a client
}
```

Clients connect with `FrameClient::connect` and get every published frame they have room for.

## Frame socket (`styx-frame-lease-v1`)

```rust
use styx::ipc::{FrameSocket, fetch_frame, lease_codec};

let socket = FrameSocket::bind("/run/styx/front-frames.sock")?;
// Advertise it (an Orion `ResourceEndpoint::Custom`: scheme `styx-frame-lease+unix`, the
// path as payload): "styx-frame-lease+unix:///run/styx/front-frames.sock".
let uri = lease_codec::endpoint_uri(socket.path());
for frame in &mut frames {
    socket.publish(&frame)?; // never blocks
}

// A consumer, in another process: the frame is leased while `frame` lives.
let frame = fetch_frame("/run/styx/front-frames.sock", Duration::from_secs(1))?;
```

Each connection gets the latest frame once, as a little-endian `u32` length, a JSON payload and
the frame's descriptors (`SCM_RIGHTS`) in one `sendmsg`. Styx owns the message and the lease:
discovery (Orion) carries only the endpoint record, never frame bytes or lease state.

**The message** is `styx_core::lease_codec` (feature `lease-codec` of `styx-core-rs`, unix;
re-exported as `styx::ipc::lease_codec`), so a consumer needs no more of Styx:

- `LeaseMessage { descriptor: FrameLeaseDescriptor, backing: LeaseBacking }`, `LeaseBacking`
  `Memfd { len }` (JSON `{"kind": "memfd", "len": ...}`) or `DmabufPlanes { planes: [LeasePlane
  { offset, len }] }` (`{"kind": "dmabuf_planes", ...}`).
- Descriptor order: `Memfd` one descriptor holding every plane at its descriptor offset;
  `DmabufPlanes` one descriptor per plane, in plane order.
- Limits: `MAX_FDS` 4 descriptors, `MAX_PAYLOAD` 64 KiB of JSON; `TRANSPORT` is
  `"styx-frame-lease-v1"`.
- `encode(&FrameLease) -> (payload, fds)` (dma-bufs and memfds as they are, other frames copied
  once into a memfd; `encode_framed` adds the length), `decode(&payload, fds) -> FrameLease`:
  checks the payload size, the descriptor count against the backing, every plane within its
  descriptor (a memfd's `len` within the memfd) and the layouts against the format, with a
  typed `LeaseCodecError`. `payload_len(header)` reads the length.
- `endpoint_uri(path)`, `parse_endpoint_uri(uri)`, `parse_endpoint(scheme, payload)`: the
  `styx-frame-lease+unix://<absolute path>` record (scheme in any case).

**Holds and flow control:**

- *Held* from the send while the consumer keeps the connection open (`fetch_frame`: while the
  returned frame lives). Consumers on the same frame share its buffer.
- *Released* when the consumer closes the connection or sends any byte (the server then
  closes it); a consumer that exits or dies has its descriptors closed by the kernel, which
  releases the same way. `FrameSocketOptions::linger` (default 0) keeps the buffer a little
  longer for consumers that close at once and read afterwards.
- *Expiry:* after `FrameSocketOptions::max_hold` (default 2 s) the server closes the connection
  (the consumer reads end of stream, or a reset if it had sent unread bytes) and lets the buffer
  go; `FrameSocketStats::revoked` counts it. The consumer's mapping stays valid, but later
  captures may write the buffer: pixels read after the cutoff may belong to newer frames. Copy a
  frame you need for longer.
- *Every buffer held:* `publish` never blocks and the capture never waits: it drops frames
  until a buffer comes back (native PiSP: `PipelineError::OutputsHeld` inside the capture), and
  the latest frame stays the same meanwhile, so fetches return it again.
  `FrameSocketOptions::max_held_frames` caps the frames held for consumers besides the latest:
  one more ends the leases on the oldest (counted as revoked).
- *Sizing:* the socket keeps the latest frame, each distinct frame a consumer holds keeps one
  more buffer, and the ISP and the capture queue keep theirs. On a native PiSP use
  `StyxConfig::native_output_buffers` ≥ N + 3, N the distinct frames held at once (at most the
  consumers; consumers fetching in the same frame period share one). N consumers each holding a
  frame across a graph tick: N + 3. The default 6 served three consumers holding 500 ms each at
  30 fps without a gap; 4 dropped frames with two ([native-stack/pipeline.md](native-stack/pipeline.md)).
- *Ordering:* one frame per connection, the newest published when it is served; nothing is
  queued, so frames between two fetches are skipped and a fetch with nothing new returns the
  same frame. The message has no sequence number: the descriptor's `timestamp` (the capture
  time) is equal for the same frame and larger for a newer one. A consumer connecting before
  any frame waits `first_frame_wait` (1 s), then the connection is closed with nothing sent.

**Hops and statistics:** a frame that carries hops (`FrameMeta::hops`: sensor, dequeue, ISP,
queue times) is sent with them and the send time as the message's last member, `"hops"`
(`LeaseMessage::hops`, a `HopRecord` with the sequence number); a frame without hops is sent as
before, and peers that do not know the member skip it. `fetch_frame` and `FrameFetcher` add the
receive and import times, so the consumer has the frame's whole path ([metrics.md](metrics.md#hops-where-each-frames-time-goes)).
A consumer fetching again and again uses a `FrameFetcher`: its buffers and the mapping of each
camera buffer it has read are kept between fetches (one allocation per fetch, the frame's lease;
no `mmap` in steady state; `LeaseMessageInline` parses without allocating). The server's
statistics (counters, lease hold times, hops of the frames sent, its process's metrics) are
`FrameSocket::metrics()`, and for other processes the sibling endpoint `<path>.stats`: connect,
write `json` or `prometheus`, read to the end (`frame_socket::fetch_metrics`,
`fetch_metrics_text`; `metrics_top --frame-socket PATH`). The frame socket itself never reads a
request: a connection to it is always a frame lease.

Each rule is a host test (`crates/styx/src/ipc/frame_socket_tests.rs`), the message bytes have a
golden test (`crates/core/src/lease_codec_tests.rs`, with and without hops), and `decode` and
the inline parse are fuzzed (target `frame_socket_message`).

## How frames travel

- **Without copying:** frames in dma-bufs (libcamera, V4L2) or memfds are sent as their file
  descriptors (`SCM_RIGHTS` on a `SOCK_SEQPACKET` socket), with format, timestamp, clock and
  crop. The client maps the same memory. A view such as the Y plane of an NV12 frame sends only
  the planes it uses.
- **Copied once:** other frames (on the heap) are copied into one memfd per frame, shared by every
  client. Plans made exportable decode into memfds and avoid this.
- **Companions:** pyramid levels travel with their frame, as their own descriptors.
- **Hops:** a frame's hops (sensor to send) go with it as a trailer of the frame message, and the
  client's receive and import times come back with its release, so the service's metrics have
  each client's whole path (`ConsumerMetrics::hops`) and the client its own
  (`FrameClient::hop_metrics`). Older peers ignore the trailers; the protocol version is
  unchanged.
- **Mapped once:** a client keeps the mapping of each buffer it has read (dma-bufs and memfds),
  so a frame in a buffer it has seen maps nothing.
- **Reading them:** frames say whether the CPU can read them and how fast, apart from where
  they live: `FrameLease::cpu_access()` is `Cached` (memory speed), `Uncached` (readable, slow:
  copy once if reading more than once) or `None`, and `can_read_planes()` follows it. A camera
  buffer from a cached dma-heap is a `Dmabuf` frame that reads `Cached`, here and in the client:
  the sender tells the client how its memory reads. On a CM5 a 1280x800 Y plane reads in 0.17 ms
  per pass either way, the same as heap memory.
- **Async:** `FrameClient::next().await` / `stream()` / `poll_next(cx)` on any executor, and
  `try_next` on the client's pollable descriptor (see
  [Without a thread per client](#without-a-thread-per-client)); `FrameClient::recv_async`
  awaits frames on Tokio (feature `async`).

## Buffers and slow clients

The sender keeps each frame's buffers (for a camera frame, the camera buffer) until the client has
dropped the frame and its companions; the client tells the sender when it does. A client holding
`max_in_flight` frames (default 2) gets no new frames until it drops one, and a client that stops
reading is skipped, so no client can hold up the camera or the others. Clients that disconnect
are forgotten, and their frames released. The camera service sizes the camera's buffers for the
frames clients may hold.

## Measured on a Raspberry Pi CM5

Camera service, two client processes each:

| Camera | Clients | Frames | Service CPU |
|---|---|---|---|
| OV9782 | luma 320x180 + luma 1280x800 | 320x200 (ISP second output) and 1280x800, 30 fps each, dma-buf, 9 ms after exposure | 3% |
| OV9782 | luma 320x180 + RGB 640x360 | 320x180 (ISP second output, YUYV decoded) and 640x360 RGB, 30 fps each, memfd, 8–9 ms | 4% |
| C270 | luma 320x180 + RGB 640x360 | decoded into memfds, the camera's rate (11–15 fps in dim light) | 2% |

| OV9782 | two H.264 640x360 viewers (one joined later) | one libx264 encode shared, 30 fps each, ~5 Mbit/s, 11 ms after exposure; the late viewer started at a keyframe | 25% (with a C270 luma client) |
| C270 | H.264 1280x720 | MJPEG decoded and encoded, at the camera's rate | 38% |

Clients used under 0.5% CPU, and nothing was copied. A reconnecting client lost ~1.6 s of
frames across a service restart. The second OV9782 client joined with one
capture restart; the first client's frames continued across it. A C270 at 640x360 takes ~1.7 s
from starting to stream to its first frame, which a client waking the camera waits for.

Frame server: OV9782 luma frames reached a second process at 30 fps as dma-bufs, 9 ms after
exposure. Publishing and receiving a 320x180 frame takes 0.05 ms on an x86-64 laptop
(`shared_perf`).

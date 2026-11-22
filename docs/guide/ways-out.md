# Ways out: the camera service and its readers

Frames leave a camera in one of two ways:

- **In process:** a `FrameLease` from the backend's `Capture` (see [cameras.md](cameras.md)).
- **To other processes:** the camera service publishes each camera's frames on a shared ring,
  and any number of readers map the same buffers. No frame is copied on the way.

Everything else in this page is a reader of that ring, or a name for it.

```
camera -> styx-service -> frame socket (styx-socket ring) -> styx_socket::Reader (Rust)
                                                          -> include/styx.h (C)
                                                          -> styxsrc (GStreamer)
                                                          -> PipeWire Video/Source node
                                                          -> styx-daedalus (graph values)
                                    Orion topic names it  -> discover, then read the ring
```

## The camera service: `styx-service`

One process per board holds every camera. `styx-service <config.toml>` reads a TOML file. Every
key except `socket_dir` is optional, and unknown keys are errors. Every key, with its default, is
in the doc comment of `crates/out/service/src/config.rs`. A systemd unit with no start limits and
restart always is in `crates/out/service/systemd/styx-service.service`.

- **Which cameras:** `cameras = []` serves every camera found. `force` and `[camera."<id>"]`
  tables pick backends and modes (see [cameras.md](cameras.md)).
- **Failure rule:** a camera that leaves or fails comes back. Its worker retries with a backoff
  that doubles from `backoff_first_ms` and holds at `backoff_cap_ms` (5 to 10 s). A manual `stop`
  ends a camera until a manual `start`. `auto_off_after` is the only automatic off.
- **Shutdown:** SIGTERM and SIGINT stop every sensor and receiver through the Lemnos contract. A
  sensor left streaming by a SIGKILL is stopped and started again once at the next start, and
  counted (`recoveries`).
- **Access:** every socket gets `socket_group` and `socket_mode` (default: the service's group,
  `660`), whatever the umask.

### The control socket

A line protocol on `<socket_dir>/control.sock` (the full table is in
`crates/out/service/src/control.rs`):

| Request | Answer |
|---|---|
| `list` | the control socket's mode and group, one `camera <id> ...` line per camera (state, backoff, rates, mode and why, buffers and reserve, ISP state, frame sockets, errors), then one `copy ...` line per copy site |
| `controls <id>` | every control with its range, default, effective value and access |
| `set <id> <control> <value> <frame>` | applies the value from that frame on |
| `getd` / `setd <id> <device-id> ...` | a device control in the device's own units |
| `frame <id>` | the last frame's id, timestamp, exposure and gain |
| `start <id>` / `stop <id>` | the manual on and off |

### The frame sockets

Each camera's `list` line names its frame socket (`socket=`, under `<socket_dir>/cameras/`), and its processed sockets when
`isp` is set (`isp0_socket=`, `isp1_socket=`, see [isp.md](isp.md#isp-in-the-camera-service)).
The service publishes the camera's own pool, so the frames readers get are the buffers the camera
filled.

## The ring: `styx-socket`

The rest of the system reads frames through this ring.

- **Handshake:** a reader connects to the socket once and gets the pool's buffers, the ring and a
  doorbell. After that each frame is one ring entry: identity, format, planes and metadata, as
  bytes. Buffers are memfds or dma-bufs. A dma-buf frame keeps its descriptor, so a GPU can
  import it.
- **Where a reader starts:** `Reader::connect` starts at the newest frame (`Start::Newest`);
  `connect_with(Start::NextNew)` starts with the next one published.
- **The producer never waits:** with no free buffer a frame is dropped and counted. A reader too
  slow for the ring's history loses frames, and counts them as skipped.
- **Holds:** a reader holds a frame for as long as it keeps its `Frame`. The camera keeps a
  reserve of free buffers (`hardware_reserve`, default half the pool, at least 2). All readers
  together may hold `buffers - hardware_reserve - 1` frames older than the newest. Past that, the
  ring's window of recent frames shrinks first. Only if that is not enough is the reader holding
  the oldest frame cut off, and it is told so (`ReadError::Ended(ConsumerFault)`).
- **Limits:** `Limits` caps the buffers one reader may hold and for how long.
- **Torn reads:** a reader cut off while it uses a frame's bytes may see the buffer reused under
  it. `Frame::read` re-checks the buffer after the use and returns `Torn` instead of the value,
  and `Frame::check` checks at a hand-off.
- **A reader that dies** loses its holds at once, and its buffers come back.

The status line per camera shows the window and the cuts: `keep=`, `kept=`, `buffers=`,
`reserve=`, `reserve_cuts=` and `bound_unmet=`.

## C: `styx-capi`

`include/styx.h` (version 1.0) gives cameras, controls and frames to C, in one of two ways, with
the same handles and calls:

- **In process** (`styx_source_local`): opens native cameras through `styx-select` on a Lemnos
  daemon.
- **Over the service** (`styx_source_service`): uses the control socket for lists and controls,
  and each camera's frame socket for frames. Planes are read where the producer wrote them.

Each call returns a status, frames are borrowed until released, and no panic crosses the
boundary. A frame socket that cuts the caller off ends the camera with `STYX_E_STOPPED`; the caller
opens it again. A torn read is `STYX_E_TORN`. In-process libcamera and V4L2 cameras are not
opened by this path; the service serves them.

## GStreamer: `styxsrc` (`styx-gst`, feature `gstreamer`)

```
GST_PLUGIN_PATH=<dir> gst-launch-1.0 styxsrc socket=<the camera's socket= path> ! ...
```

- **No copy:** each frame's memory is the frame's own memory, read-only or a dma-buf. The frame
  goes back to the producer when GStreamer frees that memory.
- **Reconnects:** with a backoff of 250 ms doubling to 5 s.
- **Formats:** MJPG frames are `image/jpeg`. A frame GStreamer has no caps name for here is
  dropped and counted.

**GPU elements downstream** must keep the buffer until their GPU read has finished. Styx cannot
see their GPU work, and on some drivers (V3D) nothing waits for the GPU on an imported dma-buf.
Waiting on the dma-buf's fence before the frame goes back is queued (GST-RELEASE-FENCE).

## PipeWire: `styx-pipewire` (feature `pipewire`)

A `Video/Source` node whose buffers are the producer's memfds, so `pipewiresrc`, browsers and the
portal get frames with no copy. Buffer `i` is ring buffer `i`. A frame is held until PipeWire
gives it back.

The node stays up while its producer is gone, and renegotiates when the producer returns with new
buffers. A frame whose consumer still holds its buffer is released and counted busy: the producer
never waits.

## Orion: `styx-orion` (feature `orion`)

Orion names a ring and never carries a frame:

- `advertise` declares an Orion topic whose one sample says where the socket is, the format and
  the pool's shape.
- `discover` finds that topic by name, and the reader then connects to the producer's socket
  directly.
- `publish` adopts the producer's own ring and pool as an Orion buffer topic, so Orion's
  subscribers hold the same buffers. A subscriber holding a frame the reserve needs is cut off and
  told (`OrionError::CutOff`). Its held frame then reads as torn.

A socket is on one board; a ring on another board needs a carrier Orion does not have yet.

## Daedalus: `styx-daedalus` (feature `daedalus`)

A frame is the `styx:frame` value of a Daedalus graph: the frame's buffer, held until the graph
drops the value, and its dma-buf, which Daedalus moves without reading.

- **In process:** make a `StyxFrame` from a `FrameLease` and hand it to the graph as a host input.
- **Out of process:** `next_value` and `latest_value` take ring frames from a
  `styx_socket::Reader` as `styx:frame` values.

The planner imports the dma-buf into a GPU device in place where the device lists that move.

## Several cameras: `styx-group`

`Grouper` takes frames from up to `N` cameras on one clock and hands out groups whose timestamps
lie within a tolerance. It is `no_std` + `alloc`.

- **Bounded:** a fixed depth per camera and per output. Everything past it is dropped, counted by
  reason, and never copied.
- **Anchors:** the oldest pending frame (`Nearest`), or the slowest camera's (`Slowest`, so a
  60 fps camera paired with a 30 fps one gives every other frame).
- **Policies:** `Strict` (every camera), `Partial` (what arrived by the deadline), and `Latest`
  (the newest complete group).

The grouper reads no clock; the caller passes the times.

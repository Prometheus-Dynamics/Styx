# Frames for Other Processes

`styx::ipc` shares frames with other processes on the same machine (Linux). A `FrameServer`
publishes frames on a Unix socket; a `FrameClient` in another process receives them as ordinary
`FrameLease`s.

```rust
use styx::ipc::FrameServer;

// The camera's process.
let server = FrameServer::bind("/run/styx/front.sock")?;
let mut frames = plan_frames(&device, &FrameRequirements::luma())?.start()?;
for frame in &mut frames {
    server.publish(&frame)?; // never blocks on a client
}
```

```rust
use styx::ipc::FrameClient;

// Any other process.
let client = FrameClient::connect("/run/styx/front.sock")?;
while let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(1)) {
    let rows = frame.luma_rows()?;
    // ...
}
```

## How frames travel

- **Without copying:** frames in dma-bufs (libcamera, V4L2) or memfds are sent as their file
  descriptors (`SCM_RIGHTS` on a `SOCK_SEQPACKET` socket), with format, timestamp, clock and
  crop. The client maps the same memory. A view such as the Y plane of an NV12 frame sends only
  the planes it uses.
- **Copied once:** frames on the heap (decoded MJPEG, CPU-scaled frames) are copied into one
  memfd per frame, shared by every client. `FrameServerStats::copied` counts them.
- **Not sent:** companions (pyramid levels). Clients that need them build their own.

## Buffers and slow clients

The server keeps each frame's buffers (for a camera frame, the camera buffer) until every
client that received it has dropped it; the client tells the server when it does. A client
holding `max_in_flight` frames (default 2) gets no new frames until it drops one, and a client
that stops reading is skipped, so no client can hold up the camera or the others.
`FrameServerStats::skipped` counts frames clients missed. Clients that disconnect are
forgotten, and their frames released.

Each held frame may hold a camera buffer: allow for `max_in_flight` × clients extra buffers
(`StyxConfig::capture_extra_buffers`) when clients hold frames for long.

## Measured on a Raspberry Pi CM5

| Source | Travels as | Rate at the client | Frame age at the client (p50) | Client CPU |
|---|---|---|---|---|
| OV9782 luma, 1280x800 | dma-buf | 30 fps | 9 ms after exposure | 0.2% |
| C270 MJPEG decoded to 320x180 luma | memfd copy | 15 fps (the camera's rate) | — | 0.2% |

On an x86-64 laptop, publishing and receiving a 320x180 frame takes 0.05 ms (`shared_perf`).

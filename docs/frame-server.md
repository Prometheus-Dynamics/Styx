# Frames for Other Processes

`styx::ipc` shares camera frames with other processes on the same machine (Linux), without
copying them.

- **`CameraService`** owns a camera. Each client process asks for the frames it needs, and the
  service plans one shared capture for all of them.
- **`FrameServer`** publishes frames your own code produces to whoever connects.

Both deliver to a `FrameClient`, which receives ordinary `FrameLease`s.

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
    &FrameRequirements::luma().output_resolution(320, 180),
)?;
print!("{}", frames.plan().unwrap_or_default()); // what the service planned for it
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
- **Idle:** a client gets frames only as fast as it drops them, so a camera nobody reads from
  idles. By default the service pauses it after 2 s (libcamera stays configured and wakes in
  ~0.1 s; V4L2 releases). `stop_when_idle` releases instead, `keep_streaming` never stops.
- **No copies:** frames the plan decodes go straight into memfds (`FramePlan::exportable`), and
  camera buffers are passed as dma-bufs. `CameraServiceStats::copied` counts exceptions.

`examples/05_apps/camera_service.rs` runs a service and clients from the command line.

## Frame server

```rust
use styx::ipc::FrameServer;

let server = FrameServer::bind("/run/styx/front.sock")?;
let mut frames = plan_frames(&device, &FrameRequirements::luma())?.start()?;
for frame in &mut frames {
    server.publish(&frame)?; // never blocks on a client
}
```

Clients connect with `FrameClient::connect` and get every published frame they have room for.

## How frames travel

- **Without copying:** frames in dma-bufs (libcamera, V4L2) or memfds are sent as their file
  descriptors (`SCM_RIGHTS` on a `SOCK_SEQPACKET` socket), with format, timestamp, clock and
  crop. The client maps the same memory. A view such as the Y plane of an NV12 frame sends only
  the planes it uses.
- **Copied once:** other frames (on the heap) are copied into one memfd per frame, shared by every
  client. Plans made exportable decode into memfds and avoid this.
- **Companions:** pyramid levels travel with their frame, as their own descriptors.
- **Async:** `FrameClient::recv_async` awaits frames on Tokio (feature `async`).

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

Clients used under 0.5% CPU, and nothing was copied. The second OV9782 client joined with one
capture restart; the first client's frames continued across it. A C270 at 640x360 takes ~1.7 s
from starting to stream to its first frame, which a client waking the camera waits for.

Frame server: OV9782 luma frames reached a second process at 30 fps as dma-bufs, 9 ms after
exposure. Publishing and receiving a 320x180 frame takes 0.05 ms on an x86-64 laptop
(`shared_perf`).

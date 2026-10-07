# Camera previews for user interfaces

`styx::preview` (feature `preview`, Linux) makes a low-latency preview of a camera for a user
interface (HeliOS's API: one per camera): small JPEG frames at a capped rate, served as MJPEG
over HTTP or as WebSocket messages, with **no effect on the camera's real consumer** (a vision
pipeline): its format, size, frame rate, latency and capture stay as they are, the preview
never restarts the capture or takes its frames, its CPU is bounded and it drops preview frames
rather than queueing them.

```rust
use styx::ipc::FrameClient;
use styx::preview::{Preview, PreviewConfig};

let preview = Preview::from_service(
    FrameClient::options("/run/styx/cameras.sock").camera("front"),
    PreviewConfig::new().size(640, 400).max_fps(15.0).quality(70),
)?;
let body = preview.mjpeg();          // a Stream of Bytes for any HTTP server
let mut viewer = preview.subscribe(); // or JPEGs one by one: recv / next().await / Stream
```

## The encode path, and why

Target: Raspberry Pi CM5 (4x Cortex-A76, PiSP ISP, **no hardware H.264 encoder**), the main
consumer a vision pipeline.

1. **The smallest frame that is free.** On a capture that starts with a free ISP output, the
   preview's request (NV12 at the preview size) is planned on the PiSP's (or libcamera's)
   **second output**: the ISP scales in hardware, no CPU. On a capture the vision client
   already runs, adding an output would restart it, so the preview gets a **share of the
   vision client's frames** instead (zero-copy, prepared once for both) and picks its
   smallest companion that still covers the preview size (a pyramid level, the scaled second
   output, the overview of a region) before scaling.
2. **CPU downscale only when needed**: 2x2 box halvings while the source is at least twice
   the preview (SIMD: `styx_core::simd::box2_row`, NEON on the CM5), then one bilinear pass to
   the exact size, straight into planar YUV 4:2:0. Chroma is scaled from its own planes (NV12's
   interleaved UV, YUYV's samples); nothing is converted to RGB. The camera frame is released
   as soon as it is scaled; the encoder works on the preview's own buffers.
3. **JPEG from YUV planes with libjpeg-turbo** (`tj3CompressFromYUVPlanes8`): no colour
   conversion, no chroma downsampling, the encoder state and output buffer kept between
   frames (`styx_codec::jpeg_planar::PlanarJpegEncoder`).
4. **Camera JPEG passes through**: a USB camera's MJPEG frames are served as they are.

Why not the others:

- **H.264/H.265**: no hardware encoder on the CM5; libx264 at 640x360 cost 25% of a core
  in the camera service ([frame-server.md](frame-server.md#measured-on-a-raspberry-pi-cm5)),
  needs keyframes for every viewer joining, and a decoder in the browser. JPEG frames stand
  alone, so dropping any of them costs nothing.
- **mozjpeg**: with libjpeg-turbo's fastest settings it is libjpeg-turbo (the same time and
  bytes, measured below) and it cannot link into one binary with it; its default profile
  (trellis quantisation, progressive scans) spends CPU for smaller files, the wrong trade for a
  preview. Available as `JpegBackend::Mozjpeg` in builds with `codec-mozjpeg` only.
- **zune-jpeg**: decodes only. **`image` (pure Rust)**: no C dependency, but it takes RGB
  (YUV converted first) and is slower (measured below); available as `JpegBackend::Image` for
  builds without C.
- **GPU (styx-gpuisp, Vulkan)**: the GPU ISP can already write NV12/I420 at half size, which
  would replace step 2 on a software-ISP capture. JPEG's entropy coding is serial and stays on
  the CPU, and at preview sizes the scale step costs well under a millisecond on the CPU
  (below), less than a Vulkan submission and fence wait. Not implemented: not clearly cheaper.

### Measured on the host

`cargo run --release -p styx-examples --features preview,image --bin preview_encode_bench`
and `--features preview-only,codec-mozjpeg` (mozjpeg and libjpeg-turbo do not link into one
binary) (x86-64, AMD Ryzen 9 5900X, a loaded machine; the C270 fixture's first
frame, stretched to 1280x800; p50 of 200 frames). The CM5 numbers are to be measured (plan
below); expect them several times these.

JPEG encode, ms per frame (p50) and bytes at q70:

| encoder, input | 640x400 q60 | q70 | q75 | bytes q70 | 320x200 q60 | q70 | q75 | bytes q70 |
|---|---|---|---|---|---|---|---|---|
| **libjpeg-turbo, I420 planes** (the preview's) | 0.29 | 0.30 | 0.30 | 10 110 | 0.076 | 0.081 | 0.077 | 3 357 |
| libjpeg-turbo, grey | 0.24 | 0.25 | 0.25 | 8 299 | 0.062 | 0.064 | 0.062 | 2 607 |
| libjpeg-turbo codec, NV12 (chroma split per frame) | 0.40 | 0.36 | 0.37 | 10 110 | 0.093 | 0.096 | 0.095 | 3 357 |
| libjpeg-turbo codec, RGB24 (converted by the encoder) | 0.46 | 0.39 | 0.40 | 10 107 | 0.104 | 0.103 | 0.103 | 3 365 |
| mozjpeg, fastest settings, raw YUV | 0.29 | 0.31 | 0.32 | 10 110 | 0.078 | 0.078 | 0.077 | 3 357 |
| `image` (pure Rust), I420 converted to RGB | 4.9 | 5.8 | 5.0 | 14 185 | 1.21 | 1.25 | 1.23 | 4 298 |
| `image`, grey | 1.30 | 1.33 | 1.32 | 8 070 | 0.32 | 0.32 | 0.32 | 2 564 |

The whole preview path on the preview thread (`Preview::offer`: source, scale, release,
encode), from a 1280x800 camera frame:

| source -> preview | scale p50 | encode p50 (p95) | bytes | CPU per frame |
|---|---|---|---|---|
| NV12 -> 640x400 q70 | 0.16–0.29 ms | 0.33–0.43 (0.39–0.66) ms | 10 089 | 0.49–0.70 ms |
| NV12 -> 320x200 q70 | 0.17–0.32 ms | 0.09–0.11 ms | 3 354 | 0.26–0.43 ms |
| NV12 -> 640x400 q60 | 0.16–0.31 ms | 0.30–0.44 ms | 8 593 | 0.48–0.74 ms |
| RGB24 -> 640x400 q70 (a vision client taking RGB) | 1.2–2.2 ms | 0.33–0.46 ms | 10 102 | 1.6–2.5 ms |

(Ranges: two runs on the loaded host.) At 15 fps a 640x400 preview from NV12 costs about
1% of one host core (0.5–0.7 ms x 15). `preview_server` (a virtual RGB 1280x800 camera at 30
fps, a vision client taking every frame, the preview capped at 15 fps) measured: the vision
client at 29.9 fps throughout, no restart, 15 preview fps (the other 15 dropped by the cap
before any work), encode p50 0.37 ms, 4.6 KB per frame (the virtual camera's flat picture),
6.5% of a core for the preview thread with the RGB scaler before its one-pass first halving
(2.5 ms per frame then; 1.6–2.5 ms now).

Conclusions: libjpeg-turbo from I420 planes is the cheapest encoder (~20% under the NV12 and
RGB entry points at 640x400, same bytes); mozjpeg with its fastest settings is libjpeg-turbo
(identical bytes and time) but cannot link next to it, and its default profile (trellis
quantisation, progressive scans) spends CPU for size, the wrong trade for a preview;
the pure-Rust encoder is 15–19x slower and is the fallback for builds without C. Scaling from
NV12 costs about as much as encoding; from the ISP's second output it is skipped.

## Planning: a low-priority client

`Preview::from_service` connects as a **low-priority** camera service client
(`ClientOptions::low_priority()`, `ClientPriority::Low`; see
[frame-server.md](frame-server.md#camera-service)):

- Normal clients are planned as if the low-priority ones were not there.
- A low-priority client gets **its own request** only when that leaves every normal client's
  plan exactly as it is (`same_plan`: backend, mode, frame rate, route, ISP outputs and
  formats, regions, queue) and adds no CPU step to the service; otherwise **a share of a
  normal client's frames** (the client whose format the preview accepts, the smallest frames
  that cover the preview), which the preview scales itself.
- Joining a running capture it **never restarts it**: its own request only when the running
  capture already gives it (same setup), else the share, else it is refused (and a
  reconnecting preview tries again).
- When a normal client joins, the capture restarts for it as it would without the preview;
  the preview is planned again after it (taking the ISP's second output when that is free).
- It cannot set a frame rate that restarts the capture, and is never the camera's owner while
  a normal client is connected (`ControlPolicy::owner_only`).
- Wire: a trailer on the request message; older services ignore it and serve the preview as a
  normal client. The protocol version stays 8.

A preview that came first (nobody else watching) is planned as it asked; the vision client
joining later restarts the capture once, before its first frame, and gets the plan it would
get alone.

## The preview thread

- **Sources:** `Preview::from_service(options, config)`: connected only while someone
  watches (`subscribe`, `mjpeg`), disconnected `idle_disconnect` (5 s) after the last viewer
  left, so the camera can idle; never blocks (connects in the background, reconnects across
  service restarts). `Preview::new(config)` + `offer(&frame)`: frames your code already has
  (the vision consumer's, in the same process); `offer` never waits and keeps a share of the
  frame (no copy) only until it is scaled.
- **Rate cap first:** frames over `max_fps` (default 15) are dropped on arrival, evenly spread
  (a 30 fps camera capped at 20 gives 20). Nothing else is done with them.
- **Latest frame only:** at most one frame waits; a newer one replaces it (`dropped_busy`). A
  service client drains its socket and keeps the newest.
- **Priority:** the thread lowers its own priority (`nice`, default 10); its CPU time is
  measured (`CLOCK_THREAD_CPUTIME_ID`).
- **Viewers:** `PreviewSubscriber` returns the newest JPEG it has not seen (`recv(timeout)`,
  `try_recv`, `next().await`, `Stream`), never a queue; a slow viewer skips frames.
  `PreviewFrame`: `jpeg` (`Bytes`, shared by all viewers), `sequence` (1, 2, ...), the
  capture `timestamp_ns` and its `clock`, `width`, `height`, `gray`, `passthrough`,
  `encode_us`.

`PreviewConfig`: `size(w, h)` (largest; aspect ratio kept, never upscaled, even sizes),
`max_fps`, `quality` (default 70), `encoder` (`JpegBackend::Auto`: turbojpeg, mozjpeg, image),
`nice`, `encode_unwatched`, `passthrough_jpeg`, `idle_disconnect`, `with_request` (what to ask
the service for; default NV12 at the preview size).

## Serving

No HTTP framework in Styx: the helpers produce bytes, the server is the application's.

**MJPEG** (`multipart/x-mixed-replace`, shown by an `<img>`): `preview.mjpeg()` is a
`futures_core::Stream<Item = Bytes>`, two chunks per frame (the part headers with
`Content-Length`, `X-Timestamp-Ns`, `X-Sequence`; then the JPEG, not copied). Serve it with
`Content-Type: MJPEG_CONTENT_TYPE` (`multipart/x-mixed-replace; boundary=styxpreview`);
`.fallible()` gives `Result<Bytes, Infallible>` items for hyper and axum.

**WebSocket**: one binary message per frame, `ws_message(&frame)`; `parse_ws_message` reads
one back.

| offset | size | field |
|---|---|---|
| 0 | 4 | magic `SPV1` |
| 4 | 2 | header length, u16 LE (32; a reader skips to it, later versions may add fields) |
| 6 | 1 | clock: 0 unknown, 1 monotonic, 2 boottime, 3 realtime, 4 stream-relative |
| 7 | 1 | flags: bit 0 grey, bit 1 camera JPEG passed through |
| 8 | 8 | sequence, u64 LE |
| 16 | 8 | capture timestamp, nanoseconds, u64 LE |
| 24 | 2 | width, u16 LE |
| 26 | 2 | height, u16 LE |
| 28 | 4 | JPEG length, u32 LE |
| 32 | n | the JPEG |

In a browser:

```js
ws.binaryType = "arraybuffer";
ws.onmessage = (e) => {
  const v = new DataView(e.data);
  const headerLen = v.getUint16(4, true), seq = v.getBigUint64(8, true);
  const ts = v.getBigUint64(16, true), len = v.getUint32(28, true);
  img.src = URL.createObjectURL(new Blob([new Uint8Array(e.data, headerLen, len)], { type: "image/jpeg" }));
};
```

### HeliOS integration (axum)

```rust
use axum::{body::Body, extract::{Path, State, ws::{Message, WebSocketUpgrade}},
           http::header, response::IntoResponse};
use styx::preview::{MJPEG_CONTENT_TYPE, Preview, PreviewConfig, ws_message};

// At startup, one preview per camera (connects only while someone watches).
let preview = Preview::from_service(
    FrameClient::options("/run/styx/cameras.sock").camera(name),
    PreviewConfig::new().name(name).size(640, 400).max_fps(15.0).quality(70),
)?;

async fn mjpeg(State(p): State<Arc<Preview>>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, MJPEG_CONTENT_TYPE), (header::CACHE_CONTROL, "no-cache")],
     Body::from_stream(p.mjpeg().fallible()))
}

async fn ws(State(p): State<Arc<Preview>>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        let mut viewer = p.subscribe();
        // Latest frame semantics: `send` awaits the client, frames in between are skipped.
        while let Some(frame) = viewer.next().await {
            if socket.send(Message::Binary(ws_message(&frame))).await.is_err() { break; }
        }
    })
}
```

`examples/05_apps/preview_server.rs` (`--features preview`) does the same with a tiny std
HTTP server: a camera service with a virtual 1280x800 camera (or `--socket`/`--camera` for a
running one), a vision client taking every full-size frame, the preview on
`http://127.0.0.1:8090/` (`/stream.mjpg`, `/frame.jpg`, `/frame.bin`, `/metrics`), and a line
of measurements each second.

## Metrics

`Preview::metrics()` and `styx::metrics::snapshot().previews` (`PreviewMetrics`), Prometheus
`styx_preview_*`: frames in, encoded, dropped by cause (rate, busy, unwatched, error), JPEG
bytes (total, per frame, p95), encode and scale times, capture-to-encoded latency, CPU time of
the preview thread, subscribers ([metrics.md](metrics.md#previews)).

## Tests

- `crates/styx/tests/preview.rs`: a vision client of the camera service and a preview whose
  own request would have moved the capture to another mode: no restart, the vision client's
  plan unchanged (the service's plan text), its frames the same size and rate (more than 80%
  of its rate alone, no gap over 250 ms), the preview at its cap from the vision client's
  frames; a preview that came first gives way to the vision client (one restart before its
  first frame, the plan it gets alone); an unwatched preview disconnects and comes back.
- `crates/styx/src/preview/tests.rs`: latest-frame dropping (200 frames offered faster than
  they encode: every one either encoded or replaced, the newest always encoded, a viewer
  gets the newest, not a queue), the rate cap, MJPEG passthrough, the multipart body, the
  WebSocket message layout; `scale.rs`: every input format, companions chosen.
- `crates/styx/src/ipc/service/camera/priority.rs`: low-priority planning never changes a
  normal plan; shares chosen by format and size. `ipc/wire/tests.rs`: the priority trailer.

## CM5 measurement plan

On helios (CM5, OV9782 on the native stack and through libcamera), release builds:

1. **Encoder cost:** `preview_encode_bench --frames 300` (`--features preview,image`, and
   `--features preview-only,codec-mozjpeg` for mozjpeg, which does not link next to
   libjpeg-turbo): p50/p95 ms and bytes for 640x400 and 320x200 at q60/70/75, turbojpeg I420 vs
   the NV12 and RGB codec paths; the whole path (scale + encode) and its CPU per frame.
2. **Vision unaffected:** the camera service with the OV9782 (`camera_service serve`), a
   vision client `Frames::nv12().size(1280, 800)` at 30/60 fps recording its rate, sensor
   sequence gaps (`metrics_top --service`) and sensor-to-receive latency (p50/p95/max) for
   60 s alone, then 60 s with `preview_server --socket ... --camera ov9782` and a browser on
   `/stream.mjpg`. Expect: no restart (`restarts`), no new drops, latency within noise.
3. **Second output:** start the preview (with a viewer) before the vision client: the vision
   client's join restarts the capture once, and the plan puts the preview on the PiSP's
   second output (`CameraServiceHandle::plan()`: the preview's consumer on the second output;
   preview `source_size` 640x400, scale time near zero). Started after the vision client, the
   preview shares its frames instead (no restart).
4. **CPU:** `styx_preview_cpu_seconds_total` rate and `cpu_percent` at 15 fps, 640x400, q70
   (expected: one A76 core's few percent), `top -H` for the `styx-preview` thread, and the
   service process's CPU with and without the preview.
5. **Under load:** all four cores busy (`stress-ng --cpu 4`): the vision client's rate and
   latency must hold; the preview drops frames (`dropped_busy`, rate below the cap) instead.
6. **Bandwidth:** bytes per frame x fps at q60/70/75 over Wi-Fi to the UI.

# From `dev` to `next`: what replaces what

`dev` stays as the frozen reference. Whether and when `next` replaces it is the user's call; this
page only maps one onto the other. Why the rebuild was done is in [`../design.md`](../design.md).

## Crates

| `dev` | On `next` | Notes |
|---|---|---|
| `styx` (the facade: capture, IPC server, planner, replay, metrics, HTTP preview, simulators, 3D engine) | no facade. Its parts went to task crates: `styx-select`, `styx-service`, `styx-socket`, `styx-outputs`, `styx-record`, `styx-counters`, `styx-preview`, `styx-sim`, `styx-group` | the route planner and cost model, the HTTP preview and metrics servers, netcam, the minifb window, the Bevy simulator and the second simulator are not carried |
| `styx-camerad` | `styx-service` | one process per board holding every camera (dev: one service per camera) |
| `styx-capi` | `styx-capi` | rewritten, `include/styx.h` 1.0. Two sources: in process (native cameras) or over the service's sockets |
| `styx-core-rs` | `styx-frame`, `styx-format`, `styx-controls`, `styx-simd`, `styx-math` | one `FrameLease` type, slimmed |
| `styx-capture` | `styx-select` (`Capture`) | one frame, control and status surface for every backend |
| `styx-codec` | `styx-codec-api`, `styx-decode`, `styx-jpeg`, `styx-h264`, `styx-ffmpeg` | the registry tries native first, FFmpeg for coverage |
| `styx-libcamera` | `styx-libcamera` | full capture on lit-robotics libcamera-rs; no C++ shim of Styx's own. Not yet run on a camera |
| `styx-v4l2` | `styx-v4l2` | on Lemnos's `lemnos-v4l2` library; dev's own V4L2 bindings go |
| `styx-sensor` | Lemnos | sensors are Lemnos drivers. Styx reads their modes, delays and gain laws through `camera.sensor/1` |
| `styx-graph`, `styx-native` | `styx-native`, `styx-native-run`, `styx-native-controls` | on Lemnos's contracts; the sensor bridge kernel module goes |
| `styx-uvc` (usbfs) | `styx-native-usb` (Lemnos's `uvc` driver), or uvcvideo through `styx-v4l2` | MJPEG on the native USB path is not chosen yet |
| `styx-hal`, `styx-runtime` | not carried | the `no_std` split stays: the gate checks `styx-frame`, `-format`, `-controls`, `-math`, `-simd`, `-settings`, `-aaa`, `-controller`, `-group`, `-dng` and `nostd-smoke` on Cortex-M and RISC-V targets with no std, and on `riscv32imc` (no compare-and-swap) with their `critical-section` feature, which takes styx-frame's counts and pool handle from portable-atomic (`styx-soft` builds without std too, outside that check); the camera runtime over embedded-hal is not carried |
| `styx-algo` | `styx-aaa`, `styx-controller`, `styx-focus`, `styx-tuning` | the service runs the controller on a processed stream with `aaa = true` (see `isp.md`, 3A) |
| `styx-pipeline` | `styx-controller` with `styx-native-controls` and an ISP crate | the closed loop runs in the service (`aaa = true`); not yet run on a board |
| `styx-softisp` | `styx-soft` | same arithmetic |
| `styx-gpuisp` | `styx-gpu` | bit for bit the soft ISP's integer arithmetic |
| `styx-wgpu` (box-filter pyramid) | not carried | pyramids and crops are graph work (Daedalus) |
| `styx-rkisp` | `styx-rkisp` | on Lemnos's layouts. ISP21 and ISP30 map; **ISP32 is refused** (dev mapped it), because the library does not state its units |
| `styx-k230isp` | `styx-k230` | on Lemnos's `k230isp` layout. **Lens shading is not written** (dev packed it), because the library has no layout for its table |
| `styx-dng` | `styx-dng` | |
| `styx-math` | `styx-math` | |
| `styx-tune` | `styx-tune` | writes Styx TOML. Dev also wrote Raspberry Pi JSON; next writes TOML only |
| `gst-styx`, `pipewire-styx` (outside the workspace) | `styx-gst`, `styx-pipewire` (in the workspace, behind features) | both read the shared ring |
| dev's own frame socket | `styx-socket` (on Lemnos's `shm-*` crates) | holds, reserve, cut-off and torn reads are stated and tested |
| `styx-core-rs`'s `daedalus` feature | `styx-daedalus` | `styx:frame` in a plain dma-buf location; no dev-only Daedalus machinery |
| `styx::multicam` | `styx-group` | |
| stills (PiSP / soft ISP reprocessing) | `styx-still` | raw DNG, and processed through the soft ISP |

## Tools and examples

| `dev` | On `next` |
|---|---|
| `tools/native-spike`, `tools/pisp-spike` | not carried (board bring-up spikes) |
| `tools/native-pipeline` | `styx-measure` on the board; replay through `styx-record` and `styx_aaa::replay` |
| `tools/compare` | not carried; `styx-measure` gives next's column of a comparison |
| `tools/styx-tune`, `tools/styx-calibrate` | `styx-tune` (`styx-tune calibrate` covers black level, noise and hot pixels) |
| `tools/styx-record` (grey Y recordings) | `styx-record` (`styxrec`, any format, with metadata; MCAP import) |
| `tools/iq-compare` | not carried |
| `examples` | `examples/` (`quickstart`, `capture`, `socket_reader`, `daedalus_frames`), all with `--sim` |
| `examples/nostd-smoke` | `crates/core/nostd-smoke` |
| `examples/nostd-camera`, `examples/mcu-footprint` | not carried (they need `styx-runtime`) |

## New on `next`

- `styx-pisp`: the PiSP configuration from neutral settings, and the live back end in the service.
- `styx-isp600`.
- `styx-block`: hardware HEVC and H.264 decode on Lemnos codec blocks.
- `styx-scale`: the hardware scaler beside the software one.
- `styx-orion`, `styx-watch`, `styx-outputs`, `styx-measure`.

## Before a switch

What a consumer of `dev` has to change, and what is still open on `next`:

- **Consumers move to `next`'s frame type** (outside block X4 in `design.md`).
- **3A in the service:** it runs with `aaa = true` on a processed stream (off by default, so
  fixed-exposure runs are unchanged). Not yet run on a board. Control names change: see below.
- **Board runs owed:**
  - libcamera on a Pi camera (H6);
  - a USB webcam (H5);
  - the Rockchip, K230 and hardware codec paths;
  - colour tuning of the OV9782 from a chart.
- **Not carried, listed above:** the `no_std` camera runtime, the GPU pyramid, `iq-compare`,
  `compare`, Raspberry Pi JSON tuning output, ISP32 mapping, and K230 lens shading.

## Control names

A consumer that set controls by `dev`'s API keys sets them on the control socket
(`set <id> <control> <value> <frame>`, see `styx_service::control`) by these names. The loop's own
controls (`ae` to `flicker`) need `aaa = true`; without it they answer `err`.

| `dev` API key | Value on `dev` | Control-socket name | Value on `next` |
|---|---|---|---|
| `exposure_us` | microseconds | `exposure` | microseconds, or `auto` (back to AE) |
| `gain` | times (2.0 is 2x) | `gain` | thousandths (2000 is 2x), or `auto` (back to AE) |
| `ae` | true / false | `ae` | `on` / `off` (off holds the last values) |
| `ev` | stops | `ev` | stops, -8 to 8 |
| (none) | | `ae_exposure_max_us` | microseconds, or `auto` (the frame's longest) |
| (none) | | `ae_gain_max` | thousandths, or `auto` (the sensor's highest) |
| `fps` | frames per second | `frame_rate` | thousandths of a frame per second (mHz) |
| `awb` | true / false | `awb` | `on` / `off` |
| `red_gain`, `blue_gain` | times | `red_gain`, `blue_gain` | thousandths, or `auto`; setting one turns AWB off |
| `colour_temperature` | kelvin | `colour_temperature` with `aaa`; `white_balance` without | kelvin (1000 to 20000), or `auto` |
| `ae_flicker_mode` (a backend control) | libcamera's flicker mode | `flicker` | `off`, `50`, `60` or `auto` |
| `af_mode`, `af_trigger`, `lens_position` | | `focus` | position 0 to 1000, or `auto` |

`exposure` and `gain` set with a number while AE runs are manual from `<frame>`: every frame from
it has the value, and AE moves the other one alone. `auto` gives the control back. The status line
says what runs (`isp_3a`: `ae,awb`, `ae`, `awb`, `manual`, or `none` without the loop), and what was
asked, applied and clamped (`exposure_*`, `gain_*`).

## Porting HeliOS

HeliOS builds against `dev` 12b63eb (its image pins bcd29a7). It uses the `styx` facade and
`styx-softisp`, and neither exists on `next`, so this is a port, not a repin. Each group below is
marked **unchanged**, **renamed** (old → new), **removed** (with what replaces it), or **adopt**
(new on `next`).

### Daedalus

- `next` builds against Daedalus rev `2f9390171104d913d2d7cdbff330150433f68ee3`, from Daedalus's
  `next` branch. The package is `daedalus`, not `daedalus-rs`, and has no `daedalus-data`.
- **HeliOS must build against exactly that source.** This is Cargo's rule, not the ABI's:
  - `styx-daedalus` is linked into the consumer's own process.
  - `StyxFrame` implements `Typed` and `DmaBufBacked` from Styx's copy of Daedalus.
  - A different rev or branch is a different crate, and its traits are not those.
  - Pin the same rev, or `[patch]` both to one checkout.
- **The plugin ABI** (`daedalus-abi`, for plugins loaded at run time) is looser. It needs:
  - the same major (0);
  - a plugin minor no higher than the host's;
  - the same `FROZEN_HASH`.

### What each group became

| Group | `dev` | `next` | |
|---|---|---|---|
| Daedalus bridge | `StyxFramesPlugin`, `frame_payload`, `FRAME_TYPE_KEY`, `FrameDescriptor`, `FrameLease` | `styx_daedalus` (feature `daedalus`) has `StyxFrame` (`from_lease(lease, size)`, `from_ring(frame)`), `FRAME_KEY` (`"styx:frame"`), `dmabuf_memory()` to add to a plan, and `next_value` / `latest_value` from a ring reader. There is nothing to register: `StyxFrame` is an opaque `Typed` value. `FrameDescriptor`'s fields are `StyxFrame`'s methods. `FrameLease` is now `styx_frame::FrameLease`. | renamed |
| Frames over IPC | `FrameClient` | `styx_socket::Reader` (`connect`, `connect_with`, `next_frame`, `latest`, `next_event`, `wait`, `stats`). Find the paths with `styx_service::camera_socket(dir, id)` and `isp_socket(dir, id, k)`. | renamed |
| Camera service | `CameraService`, `CameraServiceHandle`, `SERVICE_FRAME_RATE` | `styx-service`: one process holds every camera, set up by its TOML file. The rate is `fps_milli` for each camera. | removed |
| Controls over IPC | `ControlClient` and its builder, `ControlDescriptor`, `ControlTarget`, `StandardControl`, `ControlRefusal`, `ControlEvent`, `AppliedControl`, `ClientEvent`, `IpcError` | `styx_control_client::ControlClient` (below), over the control socket's line protocol (`styx_service::control`). `StandardControl` becomes `styx_controls::Control`, and `ControlRefusal` becomes `styx_control_client::Error` (its `ErrorKind`) or `styx_select::Refusal`. | renamed |
| Planner | `Accelerators`, `Hardware::{Auto, Off, Required}`, `Delivered`, `FrameRequest.pyramid` | the per-camera `isp`, `isp_size`, `isp_second` keys and the status line | removed |
| Probe and capture | `probe_all(_with_errors)`, `probe_capture_devices`, `capture_api::{StyxConfig, CaptureRequest}` | In process: `styx_select` (`NativeLister`, `V4l2Lister`, `LibcameraLister`, `plan`, `select`, `Capture`). Out of process: the service's `list`. `StyxConfig` and `CaptureRequest` become the service's `[camera."<id>"]` table. | renamed |
| Virtual sources | `VirtualSourceConfig`, `make_virtual_device(_with_controls)` | see below | removed |
| Hotplug | `watch::{CompositeWatcher, LinuxVideoFsWatcher, WatchRuntime}` | `styx_watch::{Watch, WatchConfig, Event}`. `Watch::poll` runs on the caller's thread; there is no runtime. | renamed |
| Codecs | `codec::CodecRegistry` | `styx_codec_api::Registry`; `styx_decode::register` adds MJPEG and raw | renamed |
| Recording | `StreamRecorder`, `RecorderOptions`, `RecordingHeader`, `open_recording`, `read_header` | `styx_record::{Recorder, RecordingHeader, Replay, RecordError, import_mcap}`. There are no options. | renamed |
| Replay | `ReplaySourceConfig`, `ReplayTimestamps`, `ReplayPacing` | `styx_record::Replay`; see below | removed |
| Preview | `Preview::from_service`, `MJPEG_CONTENT_TYPE`, `ws_message` | `styx_preview::Preview` (`new`, `offer_held`, `offer`, `attach`), which publishes JPEGs on a ring | renamed; the HTTP helpers are removed |
| Metrics | `metrics::Drops` | `CameraLine` fields (`skipped`, `publish_dropped`, `lemnos_skipped`, `reserve_cuts`, `bound_unmet`, `discontinuities`, `isp_dropped`, ...) and `ReaderStats` | removed |
| Soft ISP | `styx_softisp::{IspParams, OutputBuffers, RawFormat, Scale, SoftIsp}` | `styx_soft::{SoftIsp, OutputBuffers, RawFormat, Scale, Options, params_for}`; the parameter types are in `styx_soft::params` | renamed; same arithmetic |

### Features

| HeliOS turns on (`dev`) | On `next` |
|---|---|
| `facade`, `native`, `v4l2`, `hotplug`, `raw-decoders` | no features: depend on `styx-select`, `styx-native*`, `styx-v4l2`, `styx-watch` and `styx-decode` |
| `daedalus` | `styx-daedalus/daedalus` |
| `serde`, `metrics-serde` | `serde` on `styx-controls`, `styx-format` and `styx-aaa`. The status is text, read with `parse_camera_line`. |
| `preview` | the `styx-preview` crate |
| `codec-turbojpeg` | `styx-decode/turbojpeg` (on by default) |
| `replay-mcap`, `replay-softisp` | `styx-record` (MCAP import is always in) and `styx-soft` |
| `replay-mcap-lz4` | `styx-record/mcap-lz4` (on by default): lz4 chunks on import |
| `replay-png` | not carried (see below) |
| `async`, `tracing` | not carried: the APIs block (`poll`, `wait`), and Styx logs nothing through `tracing` |

### Controls by name

The names and units are in the table in [Control names](#control-names) above.

- **Every control is per camera.** `set <id> ...` moves the camera itself, so the raw stream and
  both processed streams (`isp0`, `isp1`) see the change. No control is per stream.
- **The 3A switches are config keys** in a camera's table, not run-time controls:
  - `aaa = true` turns the loop on, and needs `isp`;
  - `tuning`;
  - `aaa_stats_every` (1 to 30, default 4);
  - `aaa_stats_rows` (1 to 8, default 4).
- **The loop's state is on the status line:** `isp_3a`, `ae_state`, `wb_gains`,
  `colour_temperature`, `awb_outside`, `aaa_stats=<every>/<rows>`.

### What HeliOS does instead of what was removed

Each item is marked **exists on next**, **consumer's job**, or **gap** (nothing on `next` does it
yet).

- **Controls (list, get, set, watch).** These are lines on the control socket. Each request is
  one line; its answer ends in `ok` or `err <why>`.
  - **List** (exists on next): send `list`. Read it with `parse_list`, then `parse_camera_line`
    for each camera.
  - **Get** (exists on next):
    - `controls <id>` gives every standard and device control with its range, step, default,
      effective range, access and value. Read it with `parse_controls_answer`.
    - `getd <id> <device-id>` gives one device control.
    - `frame <id>` gives the last frame's applied `exposure_us` and `gain_milli`.
  - **Set** (exists on next):
    - `set <id> <name> <value> <frame>` sets a standard control. The value is a number, `auto`,
      `on` or `off`.
    - `setd <id> <device-id> <value>` sets a device control.
    - A refusal carries the camera's numbers (`err OutOfRange { min, max, value }`, `NotStep`,
      `NoEntry`, `the control is read_only`).
  - **Watch** (exists on next): there is no push. Poll `list` and read `controls_seq=`. Ask
    `controls <id>` only when it moved. It moves on every set by any client, on a range change,
    and when the camera reopens.
  - **The client** (exists on next): `styx_control_client::ControlClient` does all of the above.
    - Calls: `list`, `camera`, `controls`, `controls_info`, `get`, `device`, `set`, `set_named`
      (the 3A loop's names), `set_device`, `set_device_real`, `frame`, `start` and `stop`.
    - Each request has its own connection. The service serves one connection at a time and hangs
      up on one idle for 2 s, so the client holds none between calls.
    - Each request has a timeout (5 s by default).
    - While the socket can't be reached, the client retries on a backoff: 250 ms, doubling, capped
      at 5 s. A call before the next try fails at once as `Unreachable`.
    - `wait_controls_change(id, seen, timeout)` polls `controls_seq` every 100 ms by default,
      never faster than every 20 ms, and returns the new count.
    - Errors come back typed (`ErrorKind`: `NotFound`, `Range`, `Access`, `Invalid`, ...), with the
      service's text, which carries the camera's numbers.
    - `styx-capi` uses the same client.
- **Format, size, route and pyramids** (`CameraService` and the planner):
  - **Who decides** (exists on next): the service config, for each camera. That covers:
    - the capture: `width`, `height`, `format`, `bit_depth`, `fps_milli`;
    - the processed stream: `isp_format` and `isp_size`;
    - a second output: `isp_second = "<fourcc>:<w>x<h>"`;
    - the route: `isp = auto | pisp | soft | none`.
  - **Native mode choice** (exists on next): Styx picks the native sensor mode from these keys,
    and the status line says why (`mode`, `mode_reason`, `achieved`).
  - **Which ISP ran** (exists on next): the status line says it (`isp`, `isp_fallback`,
    `isp_switches`, `isp_reason`), and so does each frame's metadata.
  - **Changing the config** (consumer's job): HeliOS writes the config file. The service does
    not re-read its file, so a change means restarting the service.
  - **One pyramid level** (exists on next): `isp0` is level 0.
  - **Two levels** (exists on next):
    - `isp_second` at half of `isp0`'s size is level 1.
    - On the PiSP that level is a 2x2 box, the same filter as the CPU's `Scale::Half`.
    - Each processed frame carries its raw frame's id and timestamp, so pair `isp0` and `isp1`
      frames by id.
  - **Three levels** (gap): HeliOS's features and maps ask for 3. A third half-size level is graph
    work: a half-size box node over `styx:frame`, in Styx's Daedalus bridge or in the graph's own
    library. Neither exists today.
  - **Levels on request** (gap): there is no run-time "N levels" request either. Levels are config.
- **Virtual sources** (exists on next):
  - **Tests and the simulator, as a real service:**
    - `styx_sim::SimRig::start()` runs a simulated Lemnos daemon with a simulated sensor and
      receiver.
    - Point the service's `lemnos_socket` at `rig.socket()`; the camera is then served exactly as
      a board camera.
    - `pause` and `run` stop and start frames; `inject_fault` injects faults.
    - The service's own tests do this.
  - **In process:** `styx_sim::SimCamera`.
  - **A program feeding frames:** a V4L2 loopback named in `virtual_nodes`.
  - **Not carried:** a Rust API to push frames into the service as a camera.
- **Playing a recording back** (`ReplayPacing`, PNG, lz4):
  - **Reading frames** (exists on next): `styx_record::Replay::open(path)` reads a `styxrec`
    file. `next_frame` gives `FrameLease<FrameMetadata>` frames with their recorded timestamps.
  - **Pacing** (consumer's job): sleep to the timestamps, or run as fast as possible.
  - **Into a graph** (exists on next): `StyxFrame::from_lease`.
  - **Into the service as a camera** (gap): no replay source exists.
  - **MCAP** (exists on next): `import_mcap` converts an MCAP file into `styxrec`.
  - **HeliOS's lz4 recordings** (exists on next): `import_mcap` reads lz4 chunks, which is what
    HeliOS's recorder writes by default.
    - This is the `mcap-lz4` feature of `styx-record`, on by default: a pure-Rust decoder, read
      side only.
    - It costs 27 KB in a stripped x86_64 release binary that imports. The camera service does
      not link `styx-record`, so it costs the service nothing.
    - A decompressed chunk must have the size it states, and its CRC when it gives one.
    - zstd chunks are still refused by name.
  - **PNG** (gap): PNG replay is not carried.
- **Preview** (exists on next, renamed):
  - `styx_preview::Preview` takes a share of each frame, encodes a small JPEG on its own thread,
    and publishes the JPEGs as a ring. It does this without a copy and without waiting.
  - Serving MJPEG over HTTP or JPEGs over a WebSocket is the consumer's job. A ring reader has
    each JPEG's bytes in place.
  - `MJPEG_CONTENT_TYPE` and `ws_message` are gone.
- **`metrics::Drops`** (exists on next, renamed): it is the camera line's fields, read with
  `parse_camera_line`: `skipped`, `publish_dropped`, `lemnos_skipped`, `reserve_cuts`,
  `bound_unmet`, `discontinuities`, `empty_frames`, and per processed stream `isp_dropped` and
  `isp_short`. A reader's own drops and cuts are in `Reader::stats()`.

### Also used by HeliOS

These names come from a survey of HeliOS's own code (backend, at its `dev`).

- **Writing MCAP** (gap, for the coordinator to decide):
  - HeliOS's recorder writes `.mcap` through `StreamRecorder`, with `RecorderOptions`,
    `RecordCompression` and `ChannelId`.
  - On `next`, `styx-record` writes only `styxrec` (`Recorder`), and reads MCAP by import.
  - Either HeliOS writes `styxrec` with `Recorder` (exists on next; its playback tools then read
    `styxrec`), or it writes MCAP itself with the `mcap` crate it already depends on (consumer's
    job), or Styx gets an MCAP writer (gap, TODO P9).
  - `DeviceIdentity { display, keys }` becomes `RecordingHeader`'s `device` and `keys`.
- **Discovery** (`probe_all`, `probe_all_with_errors`, `probe_capture_devices`, `ProbedDevice`,
  `ProbedBackend`, `BackendHandle`, `BackendKind`, `CaptureProbe`):
  - When the service owns the cameras (the usual case on `next`): its `list`, through
    `ControlClient::list`.
    - Each camera line has the id (`lemnos:<sensor>`, `usb:<port>`, `libcamera:<id>`,
      `/dev/videoN`), `owner` (the backend: `native`, `libcamera`, `v4l2`), state, health and
      sockets.
    - The id is the stable name to key resources on; a V4L2 camera's id is its node path.
  - In process: `styx_select`'s listers give `Listed { key, anchors, virtual_node }` per
    backend, and `select` gives one owner per camera with the reason.
- **Modes** (`CaptureDescriptor { modes }`, `Mode`, `CaptureStartPolicy`, `CaptureHandle`):
  - There is no mode list.
  - A native camera's mode is chosen by the service from the camera's table, and the status says
    which and why (`mode`, `format`, `bits`, `mode_reason`, `achieved`).
  - A V4L2 node's formats, sizes and intervals: `styx_v4l2::probe_node(path)`.
  - The start and retry policy is the service config's (`backoff_first_ms`, `backoff_cap_ms`,
    `auto_off_after`).
- **Frame and format types** (`prelude`, `core::prelude`, `imports::framelease`):
  - `FrameLease` → `styx_frame::FrameLease`.
  - `BufferPool` → `styx_frame::FramePool`, with `BufferSize`.
  - `plane_layout_from_dims` → `styx_frame::packed_layouts`, or `styx_format::plane_geometry`.
  - `FrameMeta` → `styx_controls::FrameMetadata`, which has the controls in force, the sensor
    timestamp, exposure, gains and the achieved rate.
  - `ColorSpace`, `FourCc`, `Interval`, `MediaFormat` and `Resolution` → `styx_format`, same
    names. `FrameRate` → `styx_format::Interval`, or millihertz (`fps_milli`) in the config.
- **Control types** (`prelude`):
  - `ControlValue` → `styx_controls::ControlValue`.
  - `ControlRect` → `styx_controls::Rect`.
  - `ControlKind` → `styx_controls::DeviceKind`.
  - `Access` → `styx_select::Access`.
  - `ControlId`, `ControlMeta` and `ControlMetadata` → a standard control is
    `styx_controls::Control` with `styx_select::StandardInfo`; a device control is its id with
    `styx_select::DeviceInfo`.
- **Receiving** (`RecvOutcome`, `Frames`):
  - `styx_socket::Reader::next_frame` returns `Ok(Some(frame))`, `Ok(None)` (nothing new; wait
    with `wait(timeout)`), or `Err(ReadError)`.
  - `ReadError::Ended(StreamEnd::ConsumerFault)` means drop every held frame and reconnect.
  - `next_event` also gives the producer's state changes.
- **Tests** that start a `CameraService` with a `VirtualSourceConfig`: start `styx_service::Service`
  on a `styx_sim::SimRig` (see above), or read a `styx_sim::SimCamera` in process.
- **`peripherals_mem_probe`** (a `CameraService` with `CodecRegistry`): the service process's
  RSS is in `styx-measure`'s report (`--service-pid`).

### New to adopt

- **The Raze camera is a Lemnos device.**
  - The OV9782, its CSI receiver and the PiSP are reached through `lemnosd` (`lemnos_socket`).
  - Styx opens no `/dev/video*` node and no media graph for the camera, and neither should
    HeliOS.
- **Stop is receiver first.**
  - The native session stops the receiver before the sensor (`9fce7b49`, see
    [board-runs.md](board-runs.md)).
  - A consumer stops a camera only through `stop <id>` or by dropping it. It never stops the
    sensor itself.
- **Timestamps** are `CLOCK_MONOTONIC` nanoseconds on every backend. This covers `FrameEntry`'s
  `timestamp_ns`, `FrameMetadata`'s `sensor_timestamp_ns`, and the `frame <id>` answer.
  libcamera's `CLOCK_BOOTTIME` is converted; a source with no clock is stamped on arrival.
- **Holds:**
  - Readers together may hold `buffers - reserve - 1` frames older than the newest. On the Raze
    with `isp` that is 1.
  - Past that, the reserve cuts one holder off, and its next read is
    `ReadError::Ended(ConsumerFault)`. Drop every frame and reconnect.
  - Read through `Frame::read`, and call `Frame::check` before handing a frame on: a frame whose
    buffer was reused answers `Torn`.
  - A frame a GPU reads is given back only after the GPU job's fence has signalled.
- **Failure policy:** a camera that drops is served again. It retries on a backoff from 250 ms,
  doubling, capped at 5 to 10 s. It turns off only with `auto_off_after`, or by hand with `stop`.

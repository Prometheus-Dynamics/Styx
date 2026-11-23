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

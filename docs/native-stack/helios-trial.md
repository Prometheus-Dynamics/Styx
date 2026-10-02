# HeliOS on the native stack (trial)

HeliOS's camera consumer, `helios-peripherals`, built against this branch with the `native`
feature and run on the CM5 next to the installed (libcamera) service, without changing the
image or its services. The question: can HeliOS drop libcamera once the native stack is
proven, and what is left before it can.

Branches: Styx `native/helios-trial` (from `native-stack` 8936fee), HeliOS `styx-native-trial`
(from `dev` c9b39bb). Device: Raspberry Pi CM5, OV9782 behind the Styx sensor bridge
(`styx-bridge.service`, embedded data), a Logitech C270 (UVC) on USB, image v2026.2.0.
Measured 2026-10-02 (device clock 2026-08-17).

## What HeliOS uses from Styx

`helios-peripherals` (`backend/src/helios/peripherals`):

* **Probing.** `probe_all_with_errors()` on every inventory refresh, plus
  `prelude::probe_libcamera()` merged in for cameras the probe did not return. Each device
  becomes a `CaptureDevice` resource: display name = `identity.display`, local name from the
  backend's `devnode`/`device` property or a hash of the display name, label `styx.backend`
  (the backend kind's name), endpoints `dev`/`v4l2` for `/dev/video*` nodes.
* **Hotplug.** `watch::{CompositeWatcher, LinuxVideoFsWatcher, WatchRuntime}` polled every
  250 ms: a change re-runs the inventory.
* **Capture.** One persistent session per capture resource, always running:
  `CaptureRequest::new(&device).backend(kind).mode(mode.id).config(StyxConfig::new()
  .capture_queue_depth(1)).start_with_policy(CaptureStartPolicy::resilient())`, the mode
  chosen by HeliOS itself (NV12 first, then other uncompressed formats, then raw, then the
  rest; the largest first; optional `HELIOS_CAPTURE_MAX_WIDTH/HEIGHT`). It asks for no frame
  rate (the mode's `interval` is whatever the descriptor lists) and sets **no camera
  controls** (AE/AWB are the backend's defaults). Frames come from `recv_async()`; a closed
  stream restarts the session after 200 ms. `health_report`, `memory_stats`,
  `runtime_memory_report` and `metrics` are logged every 30 frames.
* **Frame server.** Per camera: `FrameLease::export_or_copy_memfd()` (dma-buf planes or a
  memfd), kept as the latest frame and handed to whoever connects to
  `<stream>.json.sock` (Orion's `UnixFdFrame`: JSON descriptor + fds). helios-engine imports it
  with `FrameLease::from_dmabuf_import` / `from_memfd_import` (`styx` feature `framelease`).
  Every frame is also MJPEG-encoded for the preview socket (`FfmpegMjpegEncoder` or
  `TurbojpegEncoder`; NV12 straight, other formats through `CodecRegistry::process`) unless
  `HELIOS_CAPTURE_PREVIEW_ENCODE=0`, and a metadata file `<stream>.json` is rewritten per
  frame.

helios-engine was idle on the device (no pipeline consuming the camera), so frames were read
by `frame_lease_client` (HeliOS branch, `src/bin/frame_lease_client.rs`), which receives,
parses (engine's own transport structs: fourcc and colour as strings) and imports exactly as
`engine/src/stream_io.rs` does, and reads the pixels.

## API breaks between 54ff6e1 and native-stack

Only one, on the HeliOS side:

| break | where fixed |
|---|---|
| `BackendKind` gained `Replay` and `Native`; HeliOS matched it exhaustively (`resources/styx.rs`) | HeliOS: uses Styx's `Display`/`FromStr` for the label (they exist since before 54ff6e1), so later backends need no change. Intended change in Styx (new backends), not a regression. |

Everything else HeliOS touches compiled unchanged: probing, `ProbedDevice`/`ProbedBackend`,
`CaptureRequest`, `CaptureStartPolicy`, `RecvOutcome`, the health/memory reports, the frame
lease export/import (`FrameLeaseDescriptor::planes` is now a `SmallVec`: same serde, and
`.collect()` builds either), `CodecRegistry`, the FFmpeg/turbojpeg encoders, the watchers.
helios-engine (`framelease` feature) also checks clean against the branch. Cross-built with
`cargo build --target aarch64-unknown-linux-gnu -p helios-peripherals --release` and the
HeliOS buildroot sysroot (libcamera, turbojpeg; FFmpeg is loaded at run time).

## What broke and was fixed

| problem | cause | fix |
|---|---|---|
| no camera: `native: no sensor description for "ov9782"` | by design no description is embedded (the OV9782 one is test data, `crates/native/src/library.rs`) | trial: `STYX_SENSOR_PATH=/tmp/helios-trial/sensors`. To ship: install `ov9782.toml` as `/usr/share/styx/sensors/ov9782.toml` |
| the OV9782 resource never published: `bind ... .mjpeg.sock: path must be shorter than SUN_LEN` | HeliOS `dev`: capture ids such as `capture_device_node-local_camera-6d9ad340` make the preview socket path 108 bytes. Any camera named by a hash (libcamera ones too) hits it on `dev`; not Styx | HeliOS: socket paths that would not fit use a hashed name in the same directory |
| trial binary could not register with Orion (`missing Orion control protocol preamble`) | HeliOS `dev` pins Orion 59f91ed, the device image runs f180849 | trial only: Orion pinned to f180849 |
| after the bridge went down and up the camera came back as a new HeliOS resource | the native display name contained the subdev node (`/dev/v4l-subdev2`, `v4l-subdev3` after the rebind); HeliOS keys resources by it | Styx a901610: named by the I²C location, `ov9782 (styx bridge i2c 10-0060)` |

Found, not fixed (pre-existing or outside the trial):

* **UVC preview fails** on every frame (`format mismatch: expected RG24, got MJPG`), with the
  installed service too: HeliOS converts YUYV for the preview with `CodecRegistry::process(
  YUYV, …)`, whose first YUYV codec (sorted by implementation id) is FFmpeg's MJPEG *encoder*,
  at 54ff6e1 as now (checked with both revisions on the device). The UVC raw stream is not
  published then (the export happens after the preview). HeliOS should ask for a decoder
  (`lookup_for_output(YUYV, RG24)` or `process_preferred`).
* **Shutdown under systemd is abrupt.** HeliOS handles only SIGINT (it aborts its tasks and
  the captures stop cleanly: exit in 30 ms, no bridge timeout). systemd stops it with SIGTERM,
  which HeliOS does not handle, so the process dies with the capture running: the bridge
  waits 1 s for the stop acknowledgement and powers the sensor off (`stop request N failed:
  -110`, also after SIGKILL). The next start works (first frame published 0.54-0.56 s after
  start, three kill/restart cycles), as the native stack's killed-owner handling intends;
  HeliOS should treat SIGTERM like SIGINT.
* **Kernel oops on a rebind under a streaming process.** `systemctl stop styx-bridge`
  (`down.sh`) unbinds `rp1-cfe` while HeliOS streams from it; Styx's capture reported the loss
  (`VIDIOC_DQBUF failed: No such device`, supervisor reconnecting) and closing the stale video
  node oopsed in `csi2_stop_channel` (`cfe_stop_streaming` from `vb2_fop_release`, the rp1-cfe
  pitfall in the README), and the process could not exit (zombie, second oops, "reboot is
  needed"). The device was rebooted. `down.sh` checks for holders only on the way up; it
  should refuse (or stop the holders) on the way down. Also, `down.sh` starts the installed
  `helios-peripherals`, so "bridge down" is not a clean unplug test.
* The `v4l2` probe reports every non-camera node as a probe error (30 `WARN` lines per
  inventory refresh), as the installed service does.
* With libcamera compiled in, libcamera's own probe logs six `ERROR` lines per start about the
  bridged sensor (missing mandatory controls); harmless, gone without libcamera.

## Results on the device

All with HeliOS's environment (`/etc/default/helios-peripherals.env`: one worker thread,
`MALLOC_ARENA_MAX=2`, the Orion sockets), run from `/tmp` (working directory `/tmp`, not
`/var/lib/helios`); release builds (`opt-level = "z"`, fat LTO, as gaia builds them). Both
cameras capture in every run (the C270 at YUYV 1280x960, 7.5 fps; its broken preview encode,
above, costs about 10% of a core in every run with the preview on). "native" = this branch
with libcamera still compiled in (the OV9782 comes from the native backend: libcamera cannot
use the bridged sensor); "native only" = the same without the `libcamera` feature (links
neither libcamera nor libstdc++); "libcamera" = the installed service
(`/usr/bin/helios-peripherals`, Styx 8d09daa, Orion f180849) with the bridge down.

* **Found**: `ov9782 (styx bridge i2c 10-0060)`, backend `native`, ISP `pisp`, tuning
  `/usr/share/libcamera/ipa/rpi/pisp/ov9782.json`; NV12/RG24 modes at 1280x800 and 640x400
  next to the raw ones; HeliOS picks NV12 1280x800, as with libcamera.
* **Frames reach the consumer**: NV12 1280x800 as two dma-buf planes (offsets 0 and 1 024 000),
  received, imported and read by the engine-style client: 604 distinct frames per 10 s
  (= 60.28 fps), no transport errors, pixel checksums non-zero. The C270's YUYV frames
  (styx-kernel's V4L2 backend, no `v4l` crate) reach it too (7.45 fps, memfd) when the preview
  is off.
* **Frame rate**: HeliOS asks for none. The native backend uses the description's default
  mode rate, 60.28 fps, and keeps it whatever the light (AE stays within the frame). libcamera
  lets its AE stretch the frame in low light: 50.1 fps in the first session, 30.2 fps in the
  second (`capture_fps`, `interframe_ms` 20.15 and 33.31). The native backend takes a mode
  interval when one is asked for (any rate in the mode's stepwise range).

Two sessions, the second after the reboot (below), same scene within each:

| | native | native only | libcamera |
|---|---|---|---|
| process start → first OV9782 frame published | 0.34-0.35 s, 0.82 s | 0.36 s | 0.46-0.55 s |
| OV9782 frame rate | 60.28 fps | 60.28 fps | 50.1 fps / 30.2 fps |
| CPU, % of one core, preview on (as deployed) | 78.6-79.5 | 79.8-80.0 | 62.3-62.8 (50 fps) / 45.7-45.9 (30 fps) |
| CPU, preview off (`HELIOS_CAPTURE_PREVIEW_ENCODE=0`) | — | 3.7-3.8 (60 fps) | 1.8 (rate not logged) / 2.4 (30 fps) |
| same per OV9782 frame (C270 capture included) | — | 0.62 ms | 0.80 ms (30 fps) |
| preview MJPEG encode per OV9782 frame (HeliOS metric) | 10.6-10.7 ms | 10.6 ms | 10.1-11.0 ms |
| RSS / PSS, preview on | 64.6 / 62.9 MiB | 57.6 / 56.0 MiB | 62.3-63.1 / 56.1-56.9 MiB |
| RSS / PSS, preview off | — | 28.0 / 26.4 MiB | 37.1-37.5 / 31.7-32.2 MiB |
| threads / fds / dma-buf fds | 12 / 58-62 / 18-22 | 10 / 58-62 / 18 | 10 / 85-89 / 39 |
| frame start → frame in the consumer's hands, p50 / p95, preview on | 21.4-21.8 / 22.5-23.4 ms | 21.4 / 22.8 ms | 31.9 / 35.7 ms* |
| same, preview off | — | 10.7 / 11.7 ms | — |

\* Second session only. In the first, the installed service's frames carried a timestamp that
did not advance (548 s at an uptime of 24 718 s) after libcamera's "Camera frontend has timed
out!" at start. In the second, the client counted 605 "distinct" frames in 10 s while the
service delivered 302: the pixels of the published frame changed while it was still the
latest one (1 603 receptions in 10 s): apparently the libcamera path of Styx 8d09daa refills an
exported buffer while consumers can still read it. The native path never did (frames by
timestamp and by pixels agreed exactly).

What the numbers say:

* The preview encoder dominates the deployed service: FFmpeg MJPEG of every 1280x800 NV12
  frame takes 10.1-11 ms with either backend, so its CPU follows the frame rate. The native
  stack runs at 60.28 fps where libcamera's AE ran at 30-50 fps: more frames, more CPU, about
  the same per frame. HeliOS should ask for the rate it wants (and publish the frame before
  encoding the preview: 10.6 ms of the 21.4 ms latency is the encode).
* Capture alone: 0.62 ms per OV9782 frame through HeliOS on the native stack (export, the
  per-frame metadata file write and the C270 included), 0.80 ms on libcamera at 30 fps.
* Latency without the preview: 10.7 ms from the frame start, of which 7.4 ms is sensor readout
  and ~2.3 ms the back end job with temporal denoise (`pipeline.md`).
* Memory: without libcamera the process is 5 MiB smaller in RSS than the libcamera service
  with the preview on and 9 MiB smaller without it (28 vs 37 MiB); with both compiled in it is
  about the libcamera service's size.
* Start: first frame published 0.34-0.36 s after the process starts (0.82 s once), libcamera
  0.46-0.55 s. Most of either is HeliOS's own start (inventory, Orion registration); the
  native camera's open → first frame is 52-56 ms (`pipeline.md`).

### 15-minute run

Native build (libcamera compiled in), preview on, sampled every minute:

* CPU 78.6-79.2% every minute; 58 950 frames published in 977.9 s = 60.28 fps; the client got
  604 distinct frames per 10 s at the start and at the end; no session restarts, no stream
  closes, no errors other than libcamera's probe lines at start.
* RSS 64 972 → 65 144 kB (+172 kB, flat for the last 4 minutes), PSS 60 054 → 60 226 kB,
  12 threads, 58-62 fds, 18-22 dma-buf fds, CMA free constant (38 512 of 65 536 kB).

### Restart and hotplug

* Kill and restart (SIGKILL twice, SIGTERM once): the process exits in 90-120 ms; the next
  instance publishes its first frame 0.54-0.56 s after its start every time. The bridge logs
  `stop request N failed: -110` for each (see "Shutdown"). SIGINT (HeliOS's own shutdown)
  exits in 30 ms with a clean stop: no bridge timeout.
* UVC hotplug (`authorized` 0 then 1 on the C270): the stream stops, and frames are published
  again 2.40 s after the device is re-authorized (the watcher's 250 ms poll, enumeration,
  a new session).
* Bridge down and up under the running process: Styx reported the loss and reconnected; the
  camera came back as a new resource (fixed, a901610) and the stale node's close oopsed the
  kernel (above; the device was rebooted).

## What remains before HeliOS ships without libcamera

1. **Image content**: the bridge module, overlay and `styx-bridge.service` instead of the
   `ov9282` overlay; `ov9782.toml` in `/usr/share/styx/sensors`; the tuning in
   `/usr/share/styx/tuning` (today it is read from libcamera's directory, which goes away with
   libcamera: without it the native pipeline runs on default tuning).
2. **HeliOS**: build `helios-peripherals` without the `libcamera` feature (this branch adds
   it), stop captures on SIGTERM, ask for the frame rate it wants, fix the UVC preview codec
   lookup and the socket path length (fixed here), and publish the frame before encoding the
   preview (10.6 ms of latency today).
3. **Kernel/bridge**: unbinding `rp1-cfe` under a streaming process oopses; `down.sh` must not
   do it, and an updater or a bridge restart must stop HeliOS first.
4. **Native stack and process**: the OV9782 description's licensing status (test data until
   rewritten from the datasheet, so not embedded); a HeliOS build whose Orion matches the
   image (dev's 59f91ed does not talk to v2026.2.0's orion-node; the trial pinned f180849).
5. **Not covered by this trial**: helios-engine actually consuming the stream (it was idle;
   the client reproduces its transport and import), camera controls (HeliOS sets none today;
   AE/AWB ran on their defaults), runs longer than 15 minutes, a real CSI unplug, and the
   OV9782 at the rate HeliOS will want once it asks for one.

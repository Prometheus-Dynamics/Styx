# HeliOS on the native stack (trial)

HeliOS's camera consumer, `helios-peripherals`, built against this branch with the `native`
feature and run on the CM5 next to the installed (libcamera) service, without changing the
image or its services. The question: can HeliOS drop libcamera once the native stack is
proven, and what is left before it can.

Branches: Styx `native/helios-trial` (from `native-stack` 8936fee), HeliOS `styx-native-trial`
(from `dev` c9b39bb). Device: Raspberry Pi CM5, OV9782 behind the Styx sensor bridge
(`styx-bridge.service`, embedded data), a Logitech C270 (UVC) on USB, image v2026.2.0.
Measured 2026-10-02 (device clock 2026-08-17).

A second round the same day ([below](#second-round-ready-to-ship)) fixed what this one found,
ran HeliOS's real consumer (helios-engine) for 90 minutes, and ends with the
[checklist](#shipping-helios-without-libcamera-checklist) for an image without libcamera.

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

Status after the second round: the first three are fixed (Styx, HeliOS and `down.sh`), the
probe warnings remain, the libcamera probe lines go with libcamera.

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


## Second round: ready to ship

Branches: Styx `native/helios-ready` (from `native-stack` 0a3a3fc: the trial's fixes and a
`down.sh` that stops every camera user before unbinding `rp1-cfe`), HeliOS `styx-native-trial`
(5c0a658, 1b61323). `helios-peripherals` built without its `libcamera` feature throughout
("native only": links neither libcamera nor libstdc++, 4.7 MB), run from `/tmp/helios-ready`
with the service's environment; no `STYX_SENSOR_PATH`, nothing in `/usr/share/styx`.

### What changed

Styx:

| change | why |
|---|---|
| The OV9782 description is built in (`styx_sensor::BUILTIN_DESCRIPTIONS`), after the search path (`STYX_SENSOR_PATH`, `/etc/styx/sensors`, `/usr/{local/,}share/styx/sensors`) | its register values are cleared by their author; the first round needed `STYX_SENSOR_PATH` |
| Tuning search path (`styx_pipeline::tuning`): `STYX_TUNING`, `STYX_TUNING_PATH`, `~/.config/styx/tuning`, `/etc/styx/tuning`, `/usr/{local/,}share/styx/tuning`, then built-in tunings (the HeliOS `ov9782.json`, unchanged), then libcamera's `rpi/pisp` directories, then a built-in generic tuning | the tuning came from libcamera's directory, which goes with libcamera. On the device: `tuning=builtin:ov9782.json` |
| Codec lookups that name no kind (`lookup`, `process`, `lookup_auto`, `lookup_preferred`) prefer a decoder to an encoder; `lookup_preferred_kind`; the session's `decoder_from_registry`/`encoder_from_registry` ask for their kind | YUYV has both (FFmpeg's YUYV → MJPEG/H.264 encoders, the YUYV → RGB decoder) and "ffmpeg" sorts before "yuyv-cpu": `process(YUYV)` returned the MJPEG encoder. A Styx bug, also behind `decoder_from_registry(YUYV)` |
| `TurbojpegEncoder` takes NV12 (4:2:0) and YUYV (4:2:2) and encodes their YUV samples (`tj3CompressFromYUVPlanes8`), no colour conversion | cheaper previews (below) |
| PiSP back end output buffers are reused in release order (FIFO), not last released first | HeliOS exports the latest frame's dma-bufs and drops the lease; the buffer it had just released was the next job's output, so a consumer polling the latest frame saw 1-11% of frames change under it (the next frame's pixels with the old timestamp, 43 ms old) |
| Boot-time CM5 bridge overlay gets the embedded data pad (`styx,embedded-data`), as the runtime `-emb` overlay | an image binding the bridge at boot gets the same frames as the trial (not booted here) |

HeliOS:

| change | why |
|---|---|
| SIGTERM stops like SIGINT, and stopping now calls `CaptureHandle::stop_async` on every capture and waits (5 s, then abort) | dropping a `CaptureHandle` inside async code only signals its workers and does not wait: the first round's shutdown (abort the tasks, return), tried here for SIGTERM, exited with the sensor still streaming (`stop request N failed: -110`) twice of two. Now: exit 40-60 ms after SIGTERM, no bridge timeout. Captures of removed resources stop the same way |
| `HELIOS_CAPTURE_FPS` (default 30, `0` = the mode's default): the interval asked of each mode, exact when the mode lists it or its stepwise range holds it, else the closest | HeliOS asked for no rate (60.28 fps native, 30-50 fps libcamera). HeliOS's config has no other capture rate (`backend/.env`'s `PREFERRED_CAPTURE_FPS` is unused); OV9782 runs at 30.00 fps, the C270 at 1280x960 at its only rates (7.5 fps) |
| Preview on its own thread: the frame is published first (frame socket, metadata file: 0.05 ms per frame), then handed over; the newest frame wins; encoded only while a client is connected to the `.mjpeg.sock` (`HELIOS_CAPTURE_PREVIEW_ENCODE=always`, `0`) | consumers waited for the 10.6 ms encode; HeliOS encoded every frame whether or not anyone watched |
| turbojpeg is the default preview encoder (`HELIOS_CAPTURE_PREVIEW_ENCODER=ffmpeg` for FFmpeg) | measured below |
| Preview conversions use `lookup_for_output(input, encoder input)`; Styx feature `raw-decoders` enabled explicitly | the UVC fix on the HeliOS side; Styx's `libcamera` feature used to pull `raw-decoders` in, so without libcamera YUYV had no converter at all |
| The frame server holds the captured frame until the next one replaces it (dma-buf exports) | with the Styx FIFO fix above, the buffer a consumer is handed is not written while it is the latest one |
| helios-engine reads the transport the way peripherals writes it (`#[serde(tag = "kind")]`) | on `dev` the engine's `FrameLeaseTransportBacking` is untagged and peripherals' is tagged: every import failed (`unknown variant kind`). The image's engine (v2026.2.0) reads the tagged form; `dev` regressed |

### Preview encoders

1280x800 NV12 from the OV9782 at 30 fps, one preview client, process CPU over 15 s (capture,
frame server and metadata included), per-frame times from HeliOS's own counters:

| | encode per frame | JPEG size | `helios-peripherals` CPU |
|---|---|---|---|
| FFmpeg MJPEG (`FfmpegMjpegEncoder`, NV12 direct) | 10.51 ms | 20.5 KB | 34.4% |
| turbojpeg, quality 85 (default), YUV planes | 4.45 ms | 54.6 KB | 15.8% |
| turbojpeg, quality 70 / 50 | 4.18 / 4.08 ms | 38.1 / 31.2 KB | 15.4 / 14.7% |
| no preview client (on demand: nothing encoded) | — | — | 1.9% |

C270 YUYV 1280x960 (7.5 fps): turbojpeg 9.6 ms (4:2:2, 122 KB); FFmpeg 2.2 ms YUYV → RGB
(`yuyv-cpu`) + 15.2 ms (26 KB). Both now produce valid JPEGs (the first round's every frame
failed).

### 90 minutes with helios-engine consuming

helios-engine on the device is the image's (v2026.2.0) and executes only workloads Orion
assigns it; nothing was assigned. For the run: an Orion workload
(`helios.engine.execution.v1`, inline graph: the camera binding through the host bridge),
the node record it needs (`orion_node_record`, a trial helper: `orionctl` cannot add one), and
a trial build of `helios-engine` from the same HeliOS branch (with the transport fix) from
`/tmp` with the service's environment and a 10 ms execution interval (the default 250 ms takes
4 frames/s), the installed service and its socket stopped. The engine imports the latest frame
over the frame socket (dma-bufs) every tick and runs the graph when it is new; its
`tick_count` counts the frames it took.

Build 5c0a658 (before the buffer-reuse fixes), OV9782 NV12 1280x800 at 30 fps (PiSP, built-in
tuning), C270 YUYV 1280x960 7.5 fps, preview on demand; a preview client connected from
minute 60 to 75; an engine-style consumer (`frame_lease_client`) sampling 10 s every 10
minutes. Per-minute samples:

| | minutes 0-59 | 60-74 (preview client) | 75-89 |
|---|---|---|---|
| frames published (metadata sequence) | 29.3-31.0 /s, mean 29.99 | mean 30.07 | mean 29.89 |
| frames taken by helios-engine | 27.2-29.4 /s, mean 28.0 | 29.4 | 29.7 |
| `helios-peripherals` CPU (% of a core) | 3.6-4.0 (mean 3.7) | 13.7-18.6 (17.9) | 3.7-3.9 |
| helios-engine CPU | 9.4-11.1 | 10.8-11.6 | 10.9-11.6 |
| `helios-peripherals` RSS / PSS | 29.7 → 30.0 / 27.9 → 28.2 MB | 31.0-31.1 / 29.3 MB | 31.1 / 29.3 MB (flat) |
| threads / fds / dma-buf fds | 12 / 58 / 18 | 12 / 58-63 / 18-22 | 12 / 58 / 18 |
| helios-engine RSS / fds | 4.9 MB / 13-14 | same | same |
| CMA free | 38 524 kB throughout | | |

* Preview client: 26 999 frames in 900 s (30.00 fps), all valid JPEGs, 53.8 KB mean.
* Consumer samples: no errors; frame start → frame in the consumer's hands p50 11.6-12.0 ms.
  p95 13.0-16.4 ms in four samples and 41-42.5 ms in five: frames "seen twice" (more than 300
  distinct frames per 10 s: the pixels changed with the old timestamp), the buffer reuse fixed
  afterwards (below).
* No stream closed, no session restart, no reconnect, no kernel message during the run;
  MemAvailable fell by the size of the run's own log in `/tmp` (tmpfs).
* RSS grew 0.3 MB in the first hour, 1.1 MB when the preview started (encoder buffers), then
  stayed flat.

After the buffer-reuse fixes (Styx 8db9b8e, HeliOS 1b61323), same setup, three 10 s consumer
samples: exactly 301 distinct frames per 10 s (30.00 fps), age p50 11.6-11.8 ms, p95
12.9-13.0 ms, max 20-27 ms (before, in the same minutes: 304-310 frames, max 43 ms).
Then 30 more minutes on the final build (Styx 8db9b8e, HeliOS 1b61323), preview client from
minute 20 to 25: 30.04 frames/s published, helios-engine took 28.5-30.0 /s (mean 29.3);
`helios-peripherals` 3.6-3.8% CPU (17.0-18.2% with the preview client: 9 000 frames in 300 s,
all valid), RSS 30.0-31.1 MB (flat after the preview started), 60 fds, 20 dma-buf fds (the
held frame's two planes); helios-engine 9.3-10.9% CPU, 5.0 MB; consumer samples at minutes
10, 20, 30: 301 frames per 10 s each, p50 11.6 ms, p95 12.8-12.9 ms, max 20-24 ms; no stream
closed, no restart, no kernel message.

### Restart, unplug, bridge cycle (engine consuming)

* **SIGTERM restart**: `helios-peripherals` exits 50 ms after SIGTERM; no bridge timeout in
  the kernel log (the first round: 1 s and `stop request failed: -110` every time). Restarted,
  it publishes its first OV9782 frame 0.36 s after the start; the engine resumes on its own
  (29.4 frames/s).
* **C270 unplug/replug** (`authorized` 0, 5 s, 1): the C270's stream stops while it is away,
  frames again 1.66 s after it comes back; the OV9782 stream is not interrupted.
* **`systemctl stop styx-bridge` while streaming**: 1.14 s; `down.sh` stops
  `helios-peripherals` first (SIGINT), then unbinds: no oops, no hang (the first round: two
  oopses and a reboot). Only the known `OF: ERROR: memory leak` line of the overlay removal.
  `down.sh` then starts the installed service (libcamera, `ov9282`), which gets the camera as
  a different resource (`camera-c1f888b1`).
* **`systemctl start styx-bridge`**: 0.47 s, the installed service stopped (`Conflicts=`);
  `helios-peripherals` started again publishes the first frame 0.37 s later, the camera is
  the same resource as before (`capture_device_node-local_camera-0e2f84bc`), and the
  engine's workload (bound to it) resumes without a change (28.7 frames/s).

### Against the installed libcamera service

The installed service (Styx 8d09daa, libcamera, preview encoded on every frame with FFmpeg,
no rate asked) with the bridge down, 60 s: 48.1% CPU, RSS 63.0 MB, PSS 56.3 MB, 10 threads,
87 fds, 45 dma-buf fds. Native only, as now deployed (30 fps, preview on demand) with the
engine consuming: 3.7% CPU, RSS 30 MB, PSS 28 MB, 58 fds, 18 dma-buf fds; with a preview
client 17.9%.

### Still open

* The `v4l2` probe reports every non-camera node as a probe error (30 `WARN` lines per
  inventory refresh).
* A consumer holding a frame's dma-bufs longer than about four frame periods (the back end's
  free buffers) can still see them rewritten: the frame socket hands out fds, not leases.
  helios-engine reads the frame within its tick; a slower consumer should copy, or use
  Styx's camera service, which keeps leases across processes.
* helios-engine at a 10 ms execution interval took 27-30 of the 30 frames/s (a tick also
  publishes its session to Orion); the default 250 ms takes 4. An engine that waits for the
  next frame instead of polling would take all of them.
* `HELIOS_PERIPHERALS_WORKER_THREADS=1`: the frame server, the capture tasks and the preview
  hand-off share one tokio worker (the preview encode does not, it has its own thread).
* Not run: a boot with the bridge overlay in `config.txt` (the device binds it at runtime), a
  real CSI unplug, camera controls from HeliOS (none are set; AE/AWB run on the tuning).
## Shipping HeliOS without libcamera: checklist

What the image needs, against v2026.2.0 (Atlas Raze device package 1.0.6, HeliOS `dev`
c9b39bb). Everything here was run on the CM5 except the boot-time overlay (the device binds
the bridge at runtime, `styx-bridge.service`).

**HeliOS (branch `styx-native-trial`, 5c0a658, to bring to `dev`)**

- [ ] Styx dependency on `native-stack` with this branch merged (OV9782 description and tuning
      built in, codec lookup fix, turbojpeg NV12/YUYV).
- [ ] `helios-peripherals` built without its `libcamera` feature (`--no-default-features`, or
      drop `libcamera` from its `default`): links neither libcamera nor libstdc++.
- [ ] Styx feature `raw-decoders` on `helios-peripherals` (in the branch): Styx's `libcamera`
      feature used to bring it in; without it YUYV/BGR/RGBA previews have no converter.
- [ ] The rest of the branch: SIGTERM/stop with `stop_async`, `HELIOS_CAPTURE_FPS` (default
      30), preview thread (on demand, turbojpeg), the latest frame held while served, socket
      paths that fit `sun_path`, the engine's transport tag fix, BackendKind names via
      Display/FromStr. (The trial bins `frame_lease_client`, `preview_client`,
      `orion_node_record` need not ship.)
- [ ] Orion: `dev` pins 59f91ed, the image runs f180849; build against what the image ships.

**Device package (Atlas Raze `devices/raze/gaia`)**

- [ ] `camera.toml`: drop the `BR2_PACKAGE_LIBCAMERA`, `BR2_PACKAGE_LIBCAMERA_PIPELINE_RPI_PISP`
      overrides and the two `stage.files` (`/usr/share/libcamera/ipa/rpi/pisp/ov9782.json`,
      `/usr/share/libcamera/tuning/ov9782.overrides.json`); the tuning is built into Styx.
- [ ] `buildroot-external/packages/libcamera`, `.../libpisp`: drop (libpisp is used only by
      libcamera; Styx's PiSP code is its own). On v2026.2.0 that removes `libcamera.so`,
      `libcamera-base.so` (2.4 MB), `/usr/lib/libcamera` (IPA modules, 1.7 MB),
      `/usr/libexec/libcamera` (IPA proxies, 0.35 MB), `/usr/share/libcamera` (3.2 MB) and
      `libpisp.so` (0.5 MB). On the device only libcamera (and through it
      `helios-peripherals`) links `libyaml`, `liblttng-ust`, `libdw` (elfutils) and `libgnutls`:
      drop them too unless another package selects them. `libstdc++` stays (FFmpeg, abseil).
- [ ] `kernel.toml`: add `BR2_LINUX_KERNEL_EXT_STYX_SENSOR_BRIDGE=y` with Styx's
      `kernel-modules/styx-sensor-bridge/buildroot/{linux-ext-styx-sensor-bridge.mk,Config.ext.in}`
      and the module directory copied to `linux/styx-sensor-bridge` in the external tree
      (builds `styx_sensor_bridge.ko` with the image's kernel and `styx-sensor-bridge-cm5.dtbo`).
      `BR2_LINUX_KERNEL_EXT_OV9782` (the `ov9282` OV9782 variant) is no longer needed for the
      bridge; keep it only as a fallback (Styx drives the OV9782 under `ov9282` too, without
      embedded data).
- [ ] `raze-device.txt`: replace `dtoverlay=ov9782,cam0,clk-continuous` with
      `dtoverlay=styx-sensor-bridge-cm5,cam0,clk-continuous`, and add
      `dtoverlay=styx-cam0-i2c-fast` (cam0 I²C at 400 kHz, as on the device now). Keep
      `camera_auto_detect=0`. Ship `styx-cam0-i2c-fast.dtbo` (dts in Styx).
- [ ] The module must load at boot: with the overlay in `config.txt` it binds by its
      compatible (`styx,sensor-bridge`) once `styx_sensor_bridge.ko` is in
      `/lib/modules/$(uname -r)` with `depmod` run (Buildroot does both).
- [ ] `styx-bridge.service`, `up.sh`, `down.sh`, the runtime overlays and
      `/usr/local/lib/styx-bridge`: dev-box only (runtime overlay over an `ov9782` boot);
      not needed with the boot-time overlay.

**Files**

- Nothing is required in `/usr/share/styx`: the OV9782 description and tuning are built in.
  Optional overrides: `/usr/share/styx/sensors/ov9782.toml`, `/usr/share/styx/tuning/ov9782.json`
  (or `STYX_SENSOR_PATH`, `STYX_TUNING_PATH`).

**Services**

- [ ] `helios-peripherals.service`: unchanged otherwise (SIGTERM now stops the captures);
      optional `HELIOS_CAPTURE_FPS=` in `/etc/default/helios-peripherals.env`.
- [ ] `helios-engine.service`: `STYX_LIBCAMERA_STOP_WHEN_IDLE=1` is libcamera-only (harmless).
- [ ] Order: nothing may unbind `rp1-cfe` while `helios-peripherals` streams (kernel oops).

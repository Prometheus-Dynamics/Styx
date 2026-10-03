# Styx native stack

Styx as a stand-alone camera stack: USB and CSI cameras work with nothing but the Linux kernel
underneath. No libcamera, no `v4l` crate, no C libraries in the core. libcamera stays available
as an optional compat provider for hardware we do not cover yet.

This branch (`native-stack`) builds it. The rest of Styx (frames, planner, shared captures,
camera service, IPC, codecs) sits on top unchanged.

## Principles

1. **Everything camera-specific is Rust and data.** Sensors are description files plus Rust
   drivers over I²C. The kernel keeps only generic pieces: CSI receivers, ISPs, DMA, USB, and
   one generic sensor bridge module written once.
2. **Async first, runtime-agnostic.** Devices are pollable file descriptors; frames, controls
   and hotplug are async streams. Blocking APIs are thin wrappers. Tokio is not required.
3. **Typed, frame-accurate controls.** Exposure is a duration, gain a ratio. Every change says
   which frame it lands on; every frame reports what actually produced it.
4. **Frame rates are explicit.** fps = pixel rate / (line length × frame length). Any rate the
   sensor timing allows can be set; the achieved rate is reported per frame.
5. **Unknown cameras work first, tuning comes later.** Defaults come from what the kernel or
   the camera reports.
6. **Record and replay everything.** Raw frames, statistics and parameters per frame.

## Layers

```
Apps and services     HeliOS, detectors, recorders, PipeWire / GStreamer bridges
Styx API              FrameRequirements, async frame streams, typed controls   (exists, extended)
Planner and service   shared captures, many processes                          (exists)
Session runtime       buffers, fences, per-frame control timing
Algorithms            AE / AWB / lens shading / colour in Rust, data tuning
Device graph          nodes, ports, capabilities, providers                    styx-graph
Sensor drivers        descriptions, timing, register sequences                 styx-sensor
Kernel interfaces     V4L2, media controller, subdevs, I²C, GPIO, dma-heaps    styx-kernel
─────────────────────────────────────── kernel ───────────────────────────────────────
Receiver + ISP (upstream)   styx-sensor-bridge (generic, once)   USB (uvcvideo or usbfs)
```

## Crates and ownership

| Path | Crate | What | Owner |
|---|---|---|---|
| `crates/kernel` | `styx-kernel` | Safe Rust kernel interfaces, `libc` only | kernel agent (`v4l2`, `media`, `subdev`, `dma_heap`, `event`), bridge agent (`bus`) |
| `crates/sensor` | `styx-sensor` | Sensor descriptions, timing model, exposure/gain models, register sequences, OV9782 description; descriptions of kernel-driven sensors from their subdevice plus a small data file (`sensors/kernel/*.toml`), driven through V4L2 controls | sensor agent |
| `crates/graph` | `styx-graph` | Device graph, `Provider` trait, async reactor, mock provider | graph agent |
| `crates/pisp` | `styx-pisp` | PiSP uAPI layouts, front/back end config builders, BE tiling, statistics, device layer (feature `device`); see `pisp.md` | pisp agent |
| `crates/algo` | `styx-algo` | 3A algorithms (AE, AWB, lens shading, CCM, tone), tuning (TOML, Raspberry Pi JSON), simulator, replay; see [algorithms.md](algorithms.md) | algo agent |
| `crates/native` | `styx-native` | The runtime for bridged sensors and sensors with an upstream kernel driver (`kernel.rs`, see [adding-a-camera.md](adding-a-camera.md)): description search path, discovery, `NativeCamera` (power, mode, receiver path, buffers, async frames, embedded data), frame-accurate typed controls, the `native` `Provider`; `styx` backend `BackendKind::Native` (feature `native`) | provider agent |
| `crates/softisp` | `styx-softisp` | Software ISP: unpack, black level, gains, lens shading, demosaic, CCM, tone, RGB/YUV/luma, 3A statistics; SIMD row kernels. Backs `styx-codec`'s Bayer decoders | softisp agent |
| `crates/pipeline` | `styx-pipeline` | The native processing pipeline: statistics conversion, the deterministic 3A loop runner (`Controller`), ISP settings for the PiSP and the software ISP, the PiSP and software paths on a native camera (feature `device`), raw recordings and a virtual sensor for host replays; see [pipeline.md](pipeline.md) | pipeline agent |
| `tools/compare` | `styx-compare` | Same capture through the libcamera and native backends: start latency (first frame, AE converged, exposure settled), rate and jitter, drops, CPU (with the IPA proxy), RSS/PSS and dma-bufs, frame statistics; JSON and markdown. `device-run.sh` runs the set on the CM5 | compare agent |
| `crates/uvc` | `styx-uvc` | USB Video Class cameras from userspace over usbfs (`styx-kernel::usbfs`, `uevent`): descriptors, PROBE/COMMIT, isochronous/bulk streaming, frame assembly, PTS/SCR timestamps, controls, hotplug; `styx` backend `BackendKind::Uvc` (feature `uvc`); see [uvc.md](../uvc.md) | usb-uvc agent |
| `kernel-modules/styx-sensor-bridge` | (C, GPL-2.0) | The generic sensor bridge module, overlay template, build scripts | bridge agent |
| `kernel-modules/pispbe` | (C, GPL-2.0) | The Raspberry Pi PiSP back end driver (`pisp_be`) patched for a cheaper per-job config write (117 → 8 µs), build and install scripts | pisp agent |

Crates must not depend on each other except: `styx-sensor` may use `styx-kernel` types behind
its `bus` trait implementation feature, and `styx-graph` and `styx-algo` depend on nothing new. `styx-pipeline`
is where the pieces meet (`styx-algo`, `styx-pisp`, `styx-softisp`, `styx-sensor`; `styx-native`
with its `device` feature), and `styx`'s `native` feature uses it for processed native modes.

## Key contracts

### Sensor bridge (kernel ⇄ userspace)

The bridge module registers a V4L2 subdevice bound through the device tree to the CSI receiver.
It has no sensor knowledge:

- Userspace sets the source pad format with the normal subdev format ioctls; the bridge accepts
  any format from a list given in the device tree (or all Bayer/mono formats).
- Userspace sets `V4L2_CID_LINK_FREQ`, `V4L2_CID_PIXEL_RATE`, `V4L2_CID_HBLANK`,
  `V4L2_CID_VBLANK` so the receiver and Styx know the timing.
- When the receiver starts or stops streaming (`s_stream` / `enable_streams`), the bridge queues
  a private V4L2 event to userspace and waits (bounded) for userspace to acknowledge that the
  sensor is streaming (or stopped), so the start order the receiver needs is kept.
- Power, clocks, reset and all register access are userspace's.

The exact event ids, the acknowledgement mechanism and timeouts are defined in
`kernel-modules/styx-sensor-bridge/PROTOCOL.md` by the bridge agent; `styx-kernel::bus`
implements the userspace side.

### Sensors with a kernel driver

A sensor that already has a kernel driver (Raspberry Pi cameras, the OV9782 under `ov9282`)
runs without the bridge: found as a `MEDIA_ENT_F_CAM_SENSOR` entity, described from its
subdevice (modes, `PIXEL_RATE`/`HBLANK`/`VBLANK`, control ranges) plus an optional data file
(gain model, delays, black level, embedded data layout, tuning), and driven through its V4L2
controls on the same control schedule, scheduled by frame starts. Where a bridge exists for
the same sensor, the bridge is used. See [adding-a-camera.md](adding-a-camera.md).

### Sensor description (data)

TOML, one file per sensor: identity (name, chip-id register and value, I²C address), pixel
array, Bayer order or mono, black level, power-up sequence, init registers, modes (size,
crop, bit depth, register list, line length, frame length limits, pixel rate), exposure, gain
and frame-length register maps with formulas, control delays (frames), group-hold registers,
embedded-data layout if any, tuning file. See `crates/sensor` docs.

### Timing

`fps = pixel_rate / (line_length_pixels × frame_length_lines)`, `line_length = width + hblank`,
`frame_length = height + vblank`. Exposure is limited by frame length minus a margin. The
timing model answers: achievable fps range per mode, the vblank for a target fps, the exposure
limits at that fps.

## Rules for work on this branch

- Rust crates: no C dependencies, `libc` allowed. Unsafe only in `styx-kernel` and in the
  reactor's syscalls (`styx-graph`, `rt/sys.rs`), each block with a `// SAFETY:` comment
  (workspace lint `unsafe_op_in_unsafe_fn` is deny).
- Tests: logic is unit-tested on the host; kernel wrappers get tests that run where the device
  exists and skip cleanly where it does not (e.g. `/dev/video*`, `/dev/media*`).
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test` pass; files
  stay under 800 lines (`scripts/check-file-sizes.sh`).
- The device (`ssh root@helios`, a Raspberry Pi CM5). Never enter passwords. Work only in `/tmp`
  on the device. It is a disposable dev box: leave `/boot` (except the cam0 overlay line, which
  `camera-mode.sh` switches), the A/B images and the updater alone; anything else may be changed
  as needed (say so in your report).
  - The baseline: the device boots the shipping configuration, the bridge overlay in
    `config.txt` (`dtoverlay=styx-sensor-bridge-cm5,cam0,clk-continuous` in place of HeliOS's
    `ov9782-overlay`; `overlays/styx-sensor-bridge-cm5.dtbo`) with `styx_sensor_bridge.ko` in
    `/lib/modules/$(uname -r)/updates/` (on the root overlay's upper layer, `depmod`ed: udev
    loads it at boot). The patched back end driver `pisp-be.ko` is there too, overriding the
    image's `pisp_be` in both camera modes (`kernel-modules/pispbe/install/`; `uninstall.sh`
    goes back to the image's). The bridge is bound at boot, `ov9282` is not,
    `helios-peripherals` and
    `styx-bridge.service` are disabled. Installed by
    `kernel-modules/styx-sensor-bridge/install/install.sh` (rerun it after rebuilding the
    module, then reboot).
  - Read-only probing needs no lock.
  - **Use `scripts/with-device-lock.sh <owner> '<remote command>'` for every device run**
    (`WAIT=1` to queue, `DMESG_LOG=<file>` to stream the kernel log). It takes the lock
    atomically, runs nothing if that fails, and releases only its own lock. Hand-written
    `mkdir … ; … ; rm -rf` chains have deleted other agents' locks three times.
  - Anything that uses a camera or changes device state (stopping `helios-peripherals`,
    binding/unbinding drivers, runtime overlays via configfs, loading our modules, streaming,
    switching the camera mode, rebooting) needs the device lock, taken atomically with
    `ssh root@helios 'mkdir /tmp/styx-device-lock && echo "<agent> $(date)" > /tmp/styx-device-lock/owner'`
    (retry every 60 s while it exists; never delete someone else's lock). Chain every device step
    after the lock with `&&` or `set -e` so nothing runs if taking the lock fails. A reboot wipes
    `/tmp`, the lock with it: take it again when the device is back. Keep it only as long as
    needed, and before releasing it (`rm -rf /tmp/styx-device-lock`) restore the device to its
    baseline (`sh /usr/local/lib/styx-bridge/camera-mode.sh status` says `configured: native`,
    `running: native (bridge bound at boot)`; if not, `camera-mode.sh native` reboots into it).
  - For libcamera runs (`ov9282`, libcamera, the installed `helios-peripherals`):
    `sh /usr/local/lib/styx-bridge/camera-mode.sh libcamera` swaps the overlay line and reboots;
    `camera-mode.sh native` goes back (reboots). In libcamera mode the older runtime path still
    works (`systemctl start styx-bridge`: `spike/up.sh` with the runtime overlay over the
    `ov9782` boot overlay; `systemctl stop styx-bridge` hands the camera back); it does not
    apply in native mode. If the device does not come back after a mode switch, `config.txt`
    is on `/dev/mmcblk0p1` (vfat), the original as `config.txt.pre-styx-bridge-boot`.
  - The bridge spike (`spike/up.sh`, `native-spike`, `spike/down.sh`) is approved for use under
    the lock.
  - Never use I2C force access (`I2C_SLAVE_FORCE`, `i2ctransfer -f`) while a kernel driver owns
    the address.
  - The device has rebooted unexpectedly several times (cause unknown; there is a hardware
    watchdog). While you hold the lock, stream its kernel log to a file on the host
    (`ssh root@helios dmesg -w > <worktree>/target/helios-dmesg-<time>.log &`) so a reboot leaves
    evidence; if it reboots or hangs, stop device work and report what you were doing.
- Licensing: Styx is MIT/Apache. Register values in `ov9782.toml` come from the HeliOS
  `ov9782.c` driver; the project owner wrote it and cleared their use. Both it and the HeliOS
  OV9782 tuning (`crates/pipeline/tuning/ov9782.json`, the owner's) are built into Styx. The bridge module is
  GPL-2.0 (kernel module) and lives apart from the Rust crates.
- Commits: concise messages ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
  Never push.

## rp1-cfe pitfalls (Raspberry Pi 6.12), measured on the device

- `VIDIOC_STREAMON`/`STREAMOFF` hold the video node's lock while the bridge waits for the
  acknowledgement, and `poll` on the node (`vb2_fop_poll`) and every ioctl on it take the same
  lock. Whatever serves the bridge must not touch the node meanwhile (not even through a shared
  epoll reactor that re-polls it): `styx-native`'s event thread polls with its own `poll(2)` and
  is quiesced around both calls. Before that, every stop timed out after 1 s.
- A failing sensor `s_stream(1)` oopses the kernel (`csi2_stop_channel` on channel -1 when the
  front end is unused). The bridge therefore does not fail `STREAMON` by default and reports
  the failed start in `STYX_CID_STREAM_STATE` = 4; userspace stops the receiver
  (`PROTOCOL.md`, module parameter `report_start_errors`).
- The sensor is stopped (`s_stream(0)`, the bridge's stop request) when the *last* streaming
  node stops: with embedded data that is the embedded node, so the bridge must still be served
  until it has stopped too.
- The bridge's "power off" does not reset the OV9782 on the CM5: only `cam0_reg` (avdd) is
  switched, the digital rails are always on, and the sensor keeps its registers (it answers on
  I²C with the bridge powered off). A process killed mid-stream therefore left the sensor
  *streaming* (0x0100 = 1, lanes in HS with the continuous clock) although the bridge cut
  "power" 50 ms after; the next owner's receiver never saw an LP-11 → HS start of frame, so
  its first stream got no frame-start events and no frames (the start was acknowledged), and
  only its stop put the sensor in standby for the stream after. Bring-up now writes the
  description's `stream_off` right after the chip id, before init. `native_harden killed`
  checks it (kills a streaming owner at 15-120 fps, opens after 0, 0.2 and 2.5 s: first frame
  35 ms after open every time; all five missed before).
- Unbinding `rp1-cfe` after it bound to the bridge leaks a reference to the bridge's device
  tree node (no `v4l2_async_nf_cleanup`): removing the overlay logs "OF: ERROR: memory leak".

## Phases

0. **Spike + foundations** (done): kernel interface layer, sensor description and timing, device
   graph and async core, the bridge module. Gate met: raw OV9782 frames from `rp1-cfe` with the
   sensor driven from Rust, 30/60/120 fps within 0.11% landing on the predicted frames, no
   sensor code in the kernel. Exposure unverified (dark scene).
1. **Stand-alone**: Styx's V4L2 backend and probing on `styx-kernel` (the `v4l` crate removed);
   UVC cameras work with no libcamera.
2. **Sensors and timing**: data-driven sensors through the bridge, frame-exact typed controls.
3. **Pi ISP natively**: front end statistics, back end processing.
4. **Algorithms**: AE/AWB/lens shading/colour in Rust, compared with libcamera.
5. **Extensibility and ecosystem**: software ISP, userspace UVC, PipeWire/GStreamer bridges;
   libcamera optional. Checked: no workspace crate's default build, nor `styx` with `native` and
   `v4l2`, has libcamera in its dependency graph (`scripts/check-feature-combinations.sh`
   fails if one does; CI builds and tests the native stack in a job without libcamera
   installed); the workspace and `styx --features native` build with libcamera hidden from
   pkg-config; on the CM5 a `native,v4l2` build probes the bridged OV9782 and a UVC camera,
   plans and streams, linking no libcamera. The `v4l` crate is gone.

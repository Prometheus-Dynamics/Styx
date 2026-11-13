# Styx `next`: the design

Styx is a media system: cameras in, frames out, with image processing and codecs in between. It is
its own system, not a wrapper. It runs **native first**: on Lemnos drivers, with its own 3A and
ISP. It keeps **libcamera, V4L2 and FFmpeg for coverage**: if a camera works with libcamera or plain
V4L2, it works in Styx, and native drivers are added over time. Every backend gives the same frame
type, controls and metadata, so an app never knows which one is running.

`dev` is the frozen reference. Proposal page with the full map:
https://claude.ai/artifact/JEUHSEReogrkXEWXy2X6pC
How it fits with Lemnos, Orion and Daedalus: https://claude.ai/artifact/X2xUNjzcFukqAUWbRVKXBN

## Why a rebuild

`dev` is 251k lines of Rust. Its camera logic is good (one frame type, 3A ported from the Pi IPA,
a software and a GPU ISP, its own encoders). What is wrong is underneath and around it:

- The native hardware half is built on the sensor bridge kernel module (ruled out) and on frozen
  Lemnos `dev` crates. Lemnos `next` now has the sensor, receiver, ISP, codec and USB drivers.
- One 81k-line facade crate holds capture backends, an IPC server, a planner, replay, metrics,
  HTTP preview, two simulators and a 3D engine.
- Capture loops, simulators, pools, pyramids and lens types are written two to five times.
- Its Daedalus glue uses `dev`-only machinery (`Payload::shared_with`, `Residency`). Daedalus
  `next` has no frame type.
- Board and module names sit in code (OV9782 tuning, Raze tunables, a chip table).

## The shape

```
             apps: HeliOS, Eidos graphs, tools, GStreamer, PipeWire, C programs
                                      |
        in process  |  the shared ring (other processes; Orion optional)
                                      |
                              one FrameLease type
                                      |
  +------------------------------- Styx ---------------------------------+
  |  backends (one owner per camera)        processing                    |
  |   1. native  (Lemnos drivers)           3A + tuning (native path)     |
  |   2. libcamera (its own 3A)             ISP values into Lemnos's      |
  |   3. V4L2    (lemnos-v4l2 library)        layout builders             |
  |                                         software ISP, GPU ISP         |
  |                                         codecs: own, parsers,         |
  |                                           codec.block/1, FFmpeg       |
  +----------------------------------------------------------------------+
        |                    |                     |
   Lemnos next          libcamera        Lemnos libraries (no lemnosd):
   (sensor, receiver,                     V4L2, ISP layouts and tiling,
    ISP, codec, uvc)                      netlink, the shared ring
```

## Movers don't know what they move

Lemnos, Orion and Daedalus move opaque bytes, with an id, a time and where the bytes live (host
memory, memfd, dma-buf, GPU). Only a producer and its consumer know what the bytes mean. Styx owns
the frame type and its metadata; nothing below it gets a frame type, a frame route, or a frame
special case. A new kind of data never needs a change in a mover.

## Backends

| Order | Backend | Picked when | 3A |
|---|---|---|---|
| 1 | Native | The board file has a `camera.sensor/1` chain, or a `camera.frames/1` USB driver binds | Styx |
| 2 | libcamera | No native chain, and libcamera lists the camera | libcamera's IPA |
| 3 | V4L2 | Anything else with a capture node (UVC through uvcvideo, other boards' capture drivers), through the `lemnos-v4l2` library | the device, or Styx's software ISP for raw |

One backend owns a camera at a time. Choice is per camera, logged with the reason, and can be
forced in config. libcamera and FFmpeg are optional features, loaded only when present; Styx
builds and runs without either.

### The native path

Lemnos does no 3A and holds no tuning; it carries an opaque `u64` frame identity. Styx does the
rest, in the order the Lemnos camera contracts set:

1. Find a `camera.sensor/1` device and follow `link` to its receiver and ISP.
2. `select_mode`, configure the receiver with the mode's link, start the receiver, start the
   sensor, drop the mode's `drop_frames`.
3. Each frame: read the ISP front-end statistics for frame n, run 3A, post sensor controls for
   n + the sensor's delay with `post_values`, post the ISP job for n.
4. Hand the processed frame out as a `FrameLease`.

On another board only the receiver and ISP drivers change. ISP values (gains, LSC, CCM, gamma, output sizes) are Styx's: one mapping per ISP family from one
neutral settings type into Lemnos's layout builders. The layouts and tiling are Lemnos's.

## Frames out

`FrameLease`: buffers given once, then per-frame entries with identity, plane layout, a timestamp
on the monotonic clock (`CLOCK_MONOTONIC`, nanoseconds), and which memory the buffers live in.
Steady state allocates nothing and copies nothing.

There is one way bytes move between processes in the whole stack: **the shared ring**, owned by
Lemnos as the bottom layer (leaf crates with no Lemnos runtime: the ring, buffers handed over once,
hold marks, the doorbell, fd passing). Lemnos's frame stream, Orion's buffer topics and Styx's
camera service all use it.

| Way out | For | How |
|---|---|---|
| In process | apps and tools | a call returning a `FrameLease` |
| Shared ring | other processes, with or without Orion | the camera service hands each reader the buffers, the ring and a doorbell once over a Unix socket; one entry per frame; readers hold by marks; Styx drops, never waits. No Orion needed |
| Orion (feature `orion`) | other boards, browsers, graphs elsewhere | the same ring, registered as an Orion buffer topic. Orion adds discovery and reach, never a second path |
| Daedalus (feature `daedalus`) | graphs | Styx registers `FrameLease` as its own value (`#[derive(Type)]`, `styx:frame`) in a plain dma-buf memory location, per Daedalus `docs/foreign-values.md`. In process, frames are handed in directly; out of process, a Styx ring reader node takes them from the shared ring (Orion has no Daedalus bridge) |

The camera service is its own process, one per board, holding every camera. Preview, counters and
recording are consumers of frames, published as topics, not parts of the library. The GPU ISP takes
an optional GPU-lease hook the product fills (Lemnos `gpu-lease/1`).

## Tuning

Tuning files ship with Styx's install (`/usr/share/styx/tuning`). A board build can add one, for
example when a module has none, in `/etc/styx/tuning` or a directory named in config; an added file
wins over a bundled one of the same name. Styx's code names no module.

## What goes, what stays

- Goes: the sensor bridge stack (`native`, `sensor` driver code), the usbfs UVC stack (Lemnos `uvc`
  on the native path, uvcvideo through V4L2 otherwise), MPP/CedarC/RGA code (Lemnos blocks), dev's
  own V4L2 bindings (lemnos-v4l2), ISP register layouts (Lemnos libraries), dev's own socket (the
  shared ring), the planner's routes and cost model, HTTP preview and metrics servers, netcam, the minifb window, the
  Bevy simulator, the second simulator.
- Stays, ported: `FrameLease` and pools (slimmed), formats, SIMD kernels, 3A, the software and GPU
  ISPs, ISP parameter mappings, own JPEG and H.264 encoders, MJPEG decoders, DNG, calibration,
  recording and replay, the C API, GStreamer and PipeWire bridges, the no_std split.
- Stays, as coverage backends: libcamera, V4L2, FFmpeg.

## Rules

See `AGENTS.md`. In short: no library needs another to work (Orion, Daedalus, libcamera and
FFmpeg are features); one frame type; steady state allocates and copies nothing; no product or
board names in code; failure rule; native first, coverage second.

## Outside blocks

Asked of those repos on 2026-11-11. A PR that needs one waits for it; don't work around it.

| Id | Item | Stops |
|---|---|---|
| X1 | Lemnos `next` stays buildable at the locked commit | native path; locked by commit |
| X2 | The PiSP front end has never run through the Lemnos module (Lemnos design says so) | H1's native run on the Raze |
| X3 | Met (Daedalus J3). Daedalus `next` lists/structs as graph constants (Gaia's X7; Daedalus J3) | Styx node config in graphs; scalars until then |
| X4 | Consumers (HeliOS, Eidos) move to `next`'s frame type | nothing in Styx |
| X5 | Met at Lemnos 6c28d722 (crates `shm-*`). The shared ring: a standalone leaf crate family with neutral names, hosted in the Lemnos repository, depending on nothing of Lemnos or Orion (Orion's ring, ring-buffers, -holds, -doorbell, -latest, -time, fdpass and the sample time type move there) | F2, F3, F4 |
| X6 | Met (Lemnos I4, #139). Lemnos library halves build without lemnosd and without `device`: lemnos-v4l2, ISP layouts and tiling (pisp, rkisp, k230isp, isp600), netlink hotplug | B6, B8, D2, D4, D5, D6 |
| X7 | Met at Daedalus 90ec12e0 (crate `daedalus-dmabuf`). A Daedalus dma-buf memory location (location only, no type): Daedalus J6 | F4 |

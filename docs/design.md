# Styx `next`: the design

Styx is a media system: cameras in, frames out, with image processing and codecs in between. It is
its own system, not a wrapper. It runs **native first**: on Lemnos drivers, with its own 3A and
ISP. It keeps **libcamera, V4L2 and FFmpeg for coverage**: if a camera works with libcamera or plain
V4L2, it works in Styx, and native drivers are added over time. Every backend gives the same frame
type, controls and metadata, so an app never knows which one is running.

`dev` is the frozen reference. Proposal page with the full map:
https://claude.ai/artifact/JEUHSEReogrkXEWXy2X6pC

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
                 apps: HeliOS, Eidos graphs, tools, GStreamer, PipeWire
                                      |
       Orion topic | Styx socket | in-process call | Daedalus foreign value
                                      |
                              one FrameLease type
                                      |
  +------------------------------- Styx ---------------------------------+
  |  backends (one owner per camera)        processing                    |
  |   1. native  (Lemnos drivers)           3A + tuning (native path)     |
  |   2. libcamera (its own 3A)             ISP jobs: PiSP, rkisp, K230   |
  |   3. V4L2    (any capture node)         software ISP, GPU ISP         |
  |                                         codecs: own + codec.block/1,  |
  |                                                 FFmpeg for coverage    |
  +----------------------------------------------------------------------+
        |                    |                     |
   Lemnos next          libcamera            kernel V4L2
   (sensor, receiver,
    ISP, codec, uvc)
```

## Backends

| Order | Backend | Picked when | 3A |
|---|---|---|---|
| 1 | Native | The board file has a `camera.sensor/1` chain, or a `camera.frames/1` USB driver binds | Styx |
| 2 | libcamera | No native chain, and libcamera lists the camera | libcamera's IPA |
| 3 | V4L2 | Anything else with a capture node (UVC through uvcvideo, other boards' capture drivers) | the device, or Styx's software ISP for raw |

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

On another board only the receiver and ISP drivers change. ISP job contents (gains, LSC, CCM,
gamma, output sizes) are Styx's: one mapping per ISP family from one neutral settings type.

## Frames out

`FrameLease`: buffers given once, then per-frame entries with identity, plane layout, timestamps
and which memory the buffers live in. Steady state allocates nothing and copies nothing.

| Way out | For | How |
|---|---|---|
| Orion topic (feature `orion`) | other processes and devices | declare `camN.frames`, give Orion the buffers once, one ring entry per frame; readers hold buffers by marks; Styx drops, never waits |
| Styx socket | use without Orion | the same shape: buffers once, ring entries, hold marks |
| In process | apps and tools | a call returning a `FrameLease` |
| Daedalus (feature `daedalus`) | graphs | Daedalus `next` has no frame type. Styx registers its frame as its own value (`#[derive(Type)]`, `styx:frame`) with a `dmabuf` memory space and transfers to the GPU spaces (Daedalus `docs/foreign-values.md`) |

The camera service is its own process, one per board, holding every camera. A Daedalus source node
pulls buffers from it. Preview, metrics and recording are consumers of frames, not parts of the
library.

## Tuning

Tuning files ship with Styx's install (`/usr/share/styx/tuning`). A board build can add one, for
example when a module has none, in `/etc/styx/tuning` or a directory named in config; an added file
wins over a bundled one of the same name. Styx's code names no module.

## What goes, what stays

- Goes: the sensor bridge stack (`native`, `sensor` driver code), the usbfs UVC stack (Lemnos `uvc`
  on the native path, uvcvideo through V4L2 otherwise), MPP/CedarC/RGA code (Lemnos blocks), the
  planner's routes and cost model, HTTP preview and metrics servers, netcam, the minifb window, the
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

Marked here, not requested from those repos.

| Id | Item | Stops |
|---|---|---|
| X1 | Lemnos `next` stays buildable at the locked commit | native path; locked by commit |
| X2 | The PiSP front end has never run through the Lemnos module (Lemnos design says so) | H1's native run on the Raze |
| X3 | Daedalus `next` lists/structs as graph constants (Gaia's X7) | Styx node config in graphs; F4 uses scalars until then |
| X4 | Consumers (HeliOS, Eidos) move to `next`'s frame type | nothing in Styx |

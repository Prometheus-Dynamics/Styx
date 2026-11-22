# Cameras: the backends

A camera has one owner: one backend opens it, and every other backend that also lists it leaves
it alone. Every backend hands out `FrameLease`s with `FrameMetadata`, so a reader never needs to
know which backend runs.

| Backend | Crate | Reaches | Needs | 3A | State |
|---|---|---|---|---|---|
| Native | `styx-native`, `styx-native-run`, `styx-native-controls` | a sensor, its receiver and ISPs, as Lemnos devices | a running `lemnosd` | Styx's own (see [isp.md](isp.md#3a)) | run on a board |
| Native USB | `styx-native-usb` | a USB camera as a Lemnos `camera.frames/1` device | a running `lemnosd` with its `uvc` driver | none | untested on board |
| V4L2 | `styx-v4l2` | any V4L2 capture node (uvcvideo webcams, other capture drivers) | nothing: Lemnos's `lemnos-v4l2` is used as a library, no `lemnosd` | the driver's | host (`vivid`) |
| libcamera | `styx-libcamera` | any camera libcamera supports | the `libcamera` feature and libcamera's development files (pkg-config) | libcamera's | untested on a camera |
| Simulated | `styx-sim` | a simulated sensor, receiver and ISP on Lemnos's simulated backend, in one process | nothing | Styx's own | the tests' camera |

## Choosing the owner: `styx-select`

`styx_select::select` takes what each backend lists and returns one owner per camera, with the
reason:

1. Listings that name the same hardware (the same anchor) are one camera. A native listing
   starts a camera; a libcamera or V4L2 listing joins the camera it shares an anchor with.
2. A virtual V4L2 node (a loopback fed by another program) is used only when the config names
   it (`virtual_nodes`, or a `[camera."<node>"]` table).
3. A forced backend wins: `force` for every camera, or `force` in one camera's table, which wins
   over the global one. Forcing a backend that does not list the camera is an error that names
   both.
4. Otherwise: native if native lists it, else libcamera, else V4L2.

`styx_select::Capture` is the one frame, control and status surface for whichever backend owns
the camera. The camera service and the C API open cameras only through it.

## Native cameras

A native camera is a `camera.sensor/1` device on a Lemnos board and the chain its links lead
through: the receiver, then the ISPs. `styx_native::cameras` lists every sensor on the board as
one camera each; the hardware facts are Lemnos's, and Styx walks no sysfs or media graph.

A session (`styx_native_run::Session`) picks a sensor mode for the request and says why, then
configures and starts the receiver and the sensor in the order the Lemnos contracts set. It
drops the first frames, as many as the mode's `drop_frames` says. In the camera service:

- `width` and `height` are the smallest acceptable mode; `format` (for example `pRAA`, 10-bit
  RGGB packed) and `bit_depth` narrow the candidates. With neither set, the highest bit depth
  that reaches `fps_milli` is taken.
- A rate above the mode's ceiling is answered with the ceiling. The status line says which mode
  was chosen and why (`mode_reason`), and which rate the sensor runs at (`achieved=`).
- Exposure, gain and frame rate are posted for the frame they apply to: each is written the
  sensor's own delay ahead, so the pair lands on one frame (`styx-native-controls`). A value past
  the sensor's limit is posted as asked. The driver clamps it, and the status reports the value
  applied and the clamp (`exposure_clamp`).

Failure rule: a camera that drops comes back. A failed start, or a stream that stops producing,
is torn down and retried after a backoff that doubles from its first wait to a cap of 5 to 10 s.
Nothing gives up on its own unless the config sets `auto_off_after`. `stop` is the manual off.

## Native USB

`styx_native_usb::UsbCamera` finds a USB camera that Lemnos serves, picks a format, and streams
the memory Lemnos fills: no copy. An unplug, a failure or a Lemnos restart detaches and
re-attaches it on a backoff (250 ms doubling, capped at 5 s).

MJPEG is not chosen yet. Mapping a compressed stream needs the frame's byte length, which the
Lemnos frame does not carry to Styx today. Without the native path, the same webcam works
through V4L2 (uvcvideo).

## V4L2

`styx_v4l2::probe` lists video nodes from sysfs and opens none of them. `probe_node` fully probes
one named node: its formats, sizes, intervals, and the controls Styx maps. `Stream::start`
negotiates a format and streams it:

- DMABUF-exported buffers are handed on with no copy. MMAP buffers are copied, and each copy is
  counted in `copy_counters`.
- Timestamps are `CLOCK_MONOTONIC` nanoseconds: the driver's when it flags them monotonic,
  otherwise the arrival time.
- A stream that loses its device retries with a backoff capped at 5 s.

In the service, `buffers` is the number of buffers requested and `format` is the four-character
code.

## libcamera

Built with `styx-libcamera`'s `libcamera` feature, on lit-robotics libcamera-rs; Styx carries no
C or C++ of its own here. libcamera runs its own pipeline and 3A. Each frame is a libcamera
dma-buf, handed out with no copy:

- Multi-plane formats are handed out when their planes share one dma-buf, which is libcamera's
  usual layout. Planes in several dma-bufs are refused, naming the format.
- Times are converted from libcamera's `CLOCK_BOOTTIME` to `CLOCK_MONOTONIC` per frame.

Without the feature every call reports `Unavailable`. Not yet run on a camera: the board proof is
H6.

## The simulated camera

`styx_sim::SimCamera` runs the native path end to end in one process: `lemnosd` with simulated
sensor, receiver and ISP drivers. Frames are rendered as 10-bit RGGB at the exposure and gain the
sensor latched on that frame. The frame clock is lock-step, and control delays behave as the
sensor contract says.

`styx_sim::paced` is the same chain at the mode's frame rate, used by the tests, the examples'
`--sim` and the service's simulated board. Only exposure and gain change the image.

## Cameras coming and going: `styx-watch`

`styx_watch::Watch` merges three sources into one stream of arrivals and departures:

- **Native:** Lemnos device events. A device that goes off takes away every camera whose chain
  holds it.
- **V4L2:** kernel uevents for `video4linux` nodes. The nodes are read from sysfs and never
  opened.
- **libcamera:** a listing at start only. libcamera's binding exposes no hotplug signal, so later
  changes are not seen.

Listings that share an anchor are one camera, so one plug gives one arrival. `Watch::poll` runs
on the caller's thread; there is no thread and no polling loop of its own.

# Styx compared with libcamera, V4L2 and GStreamer

What it takes to do the same things with Styx and with the stacks people use today, what is
different underneath, and what was measured. Runnable Styx versions of every task are in
[examples/](../examples/README.md).

| | Styx | libcamera | raw V4L2 | GStreamer `v4l2src` / `libcamerasrc` |
|---|---|---|---|---|
| What it is | Rust camera stack: probing, planning, capture, ISP + 3A, sharing, codecs | C++ camera stack: pipeline handlers, IPA (3A) modules | Kernel API (ioctls on `/dev/video*`, `/dev/v4l-subdev*`) | Media framework elements over V4L2 / libcamera |
| Cameras | USB/V4L2 (`v4l2`), CSI sensors driven from Rust through one generic kernel module or through their own kernel driver (`native`), libcamera cameras (optional `libcamera` backend) | Sensors with a libcamera-compatible kernel driver on supported pipelines (Raspberry Pi, Rockchip, IPU3, simple, ...), UVC | Whatever has a kernel driver; for raw CSI sensors you write the ISP and 3A yourself | What V4L2 / libcamera give |
| Language, dependencies | Rust, `libc` only in the core | C++17, libstdc++, libyaml (on the HeliOS image also lttng-ust, libdw, gnutls); IPA modules run in-process when signed, else in a proxy process | C | C, GLib |
| Processes | one (yours), plus an optional camera service for sharing | yours, plus an IPA proxy process for isolated (unsigned) IPAs | yours | yours |

## The same tasks side by side

Camera: the CM5's OV9782 at 1280x800 NV12, 30 fps. libcamera code is against libcamera 0.6/0.7
(`ExposureTimeMode` / `AnalogueGainMode`, as on the dev box), Rust through
[`libcamera-rs`](https://crates.io/crates/libcamera) 0.7 (what Styx's own optional libcamera
backend uses).

### List cameras with their modes

Styx (`list_cameras`):

```rust
for device in styx::probe_all() {
    println!("{}", device.identity.display);
    for backend in &device.backends {
        for mode in &backend.descriptor.modes {
            // Listed rates, and for sensors Styx drives, every rate in a range.
            println!("  {} {} {:?} {:?}", backend.kind, mode.format.code,
                     mode.intervals, mode.interval_stepwise);
        }
    }
}
```

libcamera (C++): a camera's formats come from a configuration you generate for a role;
frame rate ranges are a control (`FrameDurationLimits` in `camera->controls()`), not per mode.

```cpp
CameraManager cm;
cm.start();
for (const std::shared_ptr<Camera> &camera : cm.cameras()) {
    std::unique_ptr<CameraConfiguration> config =
        camera->generateConfiguration({ StreamRole::VideoRecording });
    if (!config)
        continue;
    const StreamFormats &formats = config->at(0).formats();
    for (const PixelFormat &format : formats.pixelformats())
        for (const Size &size : formats.sizes(format))
            std::cout << camera->id() << " " << format << " " << size << "\n";
    auto fd = camera->controls().find(&controls::FrameDurationLimits);
    if (fd != camera->controls().end())
        std::cout << "frame durations (µs) " << fd->second.toString() << "\n";
}
cm.stop();
```

Raw V4L2: `VIDIOC_ENUM_FMT`, `VIDIOC_ENUM_FRAMESIZES`, `VIDIOC_ENUM_FRAMEINTERVALS` per node,
and for a CSI sensor the media graph (`MEDIA_IOC_G_TOPOLOGY`) and subdevice ioctls on top. A
sensor's frame rates are not enumerated at all: they follow from `PIXEL_RATE`, `HBLANK` and
`VBLANK`, which is what Styx computes for you (`any rate 2.100..120.626 fps`).

### Open a camera and stream NV12 at 1280x800, 30 fps

Styx: say what you need; the planner picks camera, mode and route (here the PiSP with 3A in
Rust; for a USB camera YUYV and a converter), and prints why.

```rust
let wants = FrameRequirements::formats([FourCc::NV12])
    .output_resolution(1280, 800)
    .min_fps(30)
    .priority(Priority::Power);          // exactly 30 fps where the sensor allows it
let plan = styx::planner::plan_best(&styx::probe_all(), &wants)?;
println!("{plan}");                      // steps, where they run, cost, rejected options
for frame in plan.start()?.take(90) {
    let meta = frame.meta();             // timestamp, sequence, what produced the frame
    let y = &frame.planes()[0];          // NV12 Y plane, a dma-buf mapped on first read
}
```

Or explicitly: `CaptureRequest::new(&device).backend(BackendKind::Native).mode(mode_id)
.interval(Interval::from_fps(30)?).start()?`, then `recv_blocking` / `recv_async`.

libcamera (C++), following its `simple-cam` sample:

```cpp
static std::shared_ptr<Camera> camera;

static void requestComplete(Request *request)        // on libcamera's thread
{
    if (request->status() == Request::RequestCancelled)
        return;
    FrameBuffer *buffer = request->buffers().begin()->second;
    // buffer->planes()[i].fd: dma-bufs; mmap them to read pixels
    request->reuse(Request::ReuseBuffers);
    camera->queueRequest(request);
}

int main()
{
    auto cm = std::make_unique<CameraManager>();
    cm->start();
    camera = cm->cameras()[0];
    camera->acquire();

    std::unique_ptr<CameraConfiguration> config =
        camera->generateConfiguration({ StreamRole::VideoRecording });
    StreamConfiguration &cfg = config->at(0);
    cfg.pixelFormat = formats::NV12;
    cfg.size = { 1280, 800 };
    if (config->validate() == CameraConfiguration::Invalid)   // Adjusted: check what you got
        return 1;
    camera->configure(config.get());

    FrameBufferAllocator allocator(camera);
    Stream *stream = cfg.stream();
    allocator.allocate(stream);
    std::vector<std::unique_ptr<Request>> requests;
    for (const std::unique_ptr<FrameBuffer> &buffer : allocator.buffers(stream)) {
        std::unique_ptr<Request> request = camera->createRequest();
        request->addBuffer(stream, buffer.get());
        requests.push_back(std::move(request));
    }
    camera->requestCompleted.connect(requestComplete);

    ControlList start;                                       // 30 fps: fixed frame duration
    start.set(controls::FrameDurationLimits, { INT64_C(33333), INT64_C(33333) });
    camera->start(&start);
    for (std::unique_ptr<Request> &request : requests)
        camera->queueRequest(request.get());
    // ... run an event loop; then camera->stop(), allocator.free(stream),
    // camera->release(), cm->stop()
}
```

libcamera-rs is the same model in Rust: `CameraManager::new()`, `cameras().get(0)`,
`acquire()`, `generate_configuration(&[StreamRole::VideoRecording])`, `configure`,
`FrameBufferAllocator::new(&cam).alloc(&stream)`, `create_request(None)` + `add_buffer`,
`on_request_completed(|req| ...)`, `start(Some(&controls))`, `queue_request`, and re-queueing
completed requests yourself.

Raw V4L2 for a USB camera is about the same length as the libcamera code (`S_FMT`, `S_PARM`,
`REQBUFS`, `QUERYBUF` + `mmap` or `EXPBUF`, `QBUF`, `STREAMON`, `poll` + `DQBUF` + `QBUF`);
for a CSI sensor it gets you raw Bayer frames only, with no exposure control loop, no white
balance and no ISP.

GStreamer, one line each (the Styx element is `styxsrc` from `crates/gst-styx`, see
[ecosystem.md](ecosystem.md)):

```sh
gst-launch-1.0 libcamerasrc ! video/x-raw,format=NV12,width=1280,height=800,framerate=30/1 ! fakesink
gst-launch-1.0 v4l2src device=/dev/video0 ! video/x-raw,format=YUY2,width=640,height=480 ! videoconvert ! fakesink
gst-launch-1.0 styxsrc camera=ov9782 ! video/x-raw,format=NV12,width=1280,height=800,framerate=30/1 ! fakesink
```

### Exposure, gain and white balance; per-frame metadata

Styx (`camera_controls`). On a processed (NV12/RGB) capture the 3A loop owns the sensor:
exposure time and gain fix that value for AE (both fixed = manual; 0 = automatic again), and
AE/AWB/EV/white balance are controls. Every frame says what produced it.

```rust
use styx::capture_api::native_controls as ctl;
handle.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(10_000))?;
handle.set_control(ctl::GAIN, ControlValue::Float(2.0))?;
handle.set_control(ctl::AWB_ENABLE, ControlValue::Bool(false))?;
handle.set_control(ctl::COLOUR_TEMPERATURE, ControlValue::Uint(5000))?;
let ae = handle.get_control(ctl::AE_STATE)?;           // 1 searching, 2 converged (libcamera's values)
if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_secs(1)) {
    let m = frame.meta().native().unwrap();            // NativeFrameMeta
    // m.sequence, m.exposure_ns, m.analog_gain, m.digital_gain, m.frame_duration_ns,
    // m.verified: read back from the sensor's embedded data (else predicted from the delays);
    // frame.meta().timestamp: frame start, CLOCK_MONOTONIC
}
```

On a raw capture the same exposure/gain/frame-rate controls go straight to the sensor through
the control schedule: written in the frame the sensor's delays require, landing on a predicted
frame (`styx_native::CameraControls::set_exposure` returns that frame; through the `styx`
facade the frame metadata shows it). Measured on the OV9782 (`camera_controls`): a raw exposure
change landed 2 frames after the request, a processed one 3 (the loop adds a frame).

libcamera: controls go into a Request and are applied by the pipeline handler (on the
Raspberry Pi through its own `DelayedControls`, so they land some frames later); the completed
request's metadata says what each frame was made with.

```cpp
ControlList &c = request->controls();
c.set(controls::ExposureTimeMode, controls::ExposureTimeModeManual);
c.set(controls::ExposureTime, 10000);                  // µs
c.set(controls::AnalogueGainMode, controls::AnalogueGainModeManual);
c.set(controls::AnalogueGain, 2.0f);
c.set(controls::AwbEnable, false);
c.set(controls::ColourTemperature, 5000);
camera->queueRequest(request);
// in requestComplete():
const ControlList &md = request->metadata();
std::optional<int64_t> ts = md.get(controls::SensorTimestamp);   // ns, frame start
std::optional<int32_t> exposure = md.get(controls::ExposureTime);
std::optional<float> gain = md.get(controls::AnalogueGain);
std::optional<int32_t> ae = md.get(controls::AeState);
std::optional<int32_t> ct = md.get(controls::ColourTemperature);
```

In libcamera-rs: `req.controls_mut().set(controls::ExposureTime(10_000))?` and
`req.metadata().get::<controls::SensorTimestamp>()`.

Raw V4L2 (`VIDIOC_S_CTRL` / `VIDIOC_S_EXT_CTRLS` on `V4L2_CID_EXPOSURE`, `V4L2_CID_ANALOGUE_GAIN`
on the sensor subdevice): the value is written when you call it, and nothing tells you which
frame it reached; there is no AE or AWB for raw sensors. For UVC cameras the camera's own
firmware runs AE/AWB and Styx exposes its controls as they are (`camera_controls` sets
`Brightness` on the C270 by name).

### Frame rate

| | how | effect |
|---|---|---|
| Styx, any mode | `CaptureRequest::interval(Interval::from_fps(30)?)`, or `min_fps(30)` + `Priority::Power` in the planner | exact: 30.000 fps measured (`fps = pixel rate / (line length × frame length)`, frame length chosen per rate) |
| Styx, raw capture | `set_control(FRAME_RATE, Float(60.0))` while streaming | lands on a predicted frame (`camera_controls`: 33.33 ms then 16.67 ms intervals) |
| Styx, processed capture | `reconfigure` at another rate (the loop holds the rate it started with; AE chooses exposures within it) | 72 ms from the restart to the first frame at 60 fps |
| libcamera | `FrameDurationLimits` min = max at `start()` or in a request; min < max lets AE stretch frames in low light | the HeliOS service, asking for no rate, ran at 50 then 30 fps as the light changed ([helios-trial.md](native-stack/helios-trial.md)) |
| raw V4L2 | `VIDIOC_S_PARM` for UVC; `V4L2_CID_VBLANK` on a sensor subdevice (compute it yourself) | |

### Two streams of one camera

Styx (`two_consumers`): two requirements, one capture; on the PiSP both come from one back end
pass (output 0 NV12 1280x800, output 1 RGB 640x400), on other cameras the planner converts or
scales per consumer. Each consumer has its own queue and pace.

```rust
let plan = styx::planner::plan_many(&device, &[
    FrameRequirements::formats([FourCc::NV12]).min_fps(30).priority(Priority::Power),
    FrameRequirements::formats([FourCc::RG24]).output_resolution(640, 400).min_fps(30).priority(Priority::Power),
])?;
let mut consumers = plan.start()?;       // Vec<PlannedFrames>, one per requirement
```

Measured (`two_consumers`, CM5): the recorder took all 30 fps while a detector spending 50 ms
per frame took 20 fps, both as dma-bufs from the two back end outputs. Both ask for the latest
frame only (`PlanOverrides { queue_depth: Some(1), .. }`): frames queued for a slow consumer
hold ISP buffers, and with the PiSP's 4 per output a deeper queue let the slow consumer pace
the camera for both (recorder and detector at 20 fps).

libcamera: one configuration with two roles; every request carries a buffer per stream you
want filled, and both arrive in the same completed request.

```cpp
std::unique_ptr<CameraConfiguration> config =
    camera->generateConfiguration({ StreamRole::VideoRecording, StreamRole::Viewfinder });
config->at(0).pixelFormat = formats::NV12;   config->at(0).size = { 1280, 800 };
config->at(1).pixelFormat = formats::BGR888; config->at(1).size = { 640, 400 };  // R, G, B bytes
config->validate();
camera->configure(config.get());
// allocate buffers for both streams; request->addBuffer(stream0, ...); request->addBuffer(stream1, ...)
```

Pacing two consumers at different rates is up to the application (which buffers to add to
which request, and when to re-queue).

### Sharing a camera between processes

Styx: a camera service plans one shared capture for all clients, each asking for its own
format and size, and passes frames as dma-bufs (`camera_service`); or the latest frame on a
socket with a lease per consumer (`frame_socket`, HeliOS's `styx-frame-lease-v1`).

```rust
// camera process
let service = CameraService::new(device).serve("/run/styx/front.sock")?;
// any other process
let client = FrameClient::request("/run/styx/front.sock", &FrameRequirements::luma().output_resolution(320, 200))?;
while let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(1)) { /* dma-buf frame */ }
```

Measured on the CM5 (`camera_service` with two clients, 5 s): luma 320x200 and RGB 640x400 from
one PiSP pass, both 258-260 fps, frames 4.6 ms old (p50) in the client, 0.4% and 0.8% CPU per
client, nothing copied.

libcamera: a camera is acquired by one process at a time (`Camera::acquire`, and the media
device lock across processes). Sharing goes through a service in front of it: PipeWire with
libcamera's SPA plugin (and the portal), or your own IPC with dma-buf passing.

GStreamer: `tee` within one pipeline; across processes `pipewiresink` / `pipewiresrc`, or
`shmsink` / `shmsrc` (copies).

### Async

Styx: frames are awaited (`CaptureHandle::recv_async`, `PlannedFrames::next_frame_async`), the
futures need no particular runtime (`async_capture` runs the same function under Tokio and on a
10-line `std` executor), controls have `_async` variants, hotplug is an inventory watch.
libcamera: callbacks on libcamera's internal thread (`requestCompleted`, `bufferCompleted`,
`CameraManager::cameraAdded`/`cameraRemoved` signals); getting frames into an async runtime is
the application's job (libcamera-rs: a closure that you typically forward into a channel).

## What is different underneath

| | Styx | libcamera |
|---|---|---|
| API model | Ask for frames (`FrameRequirements`); the planner chooses camera, mode, ISP or converter and prints its reasoning; or pick mode and interval yourself | Configure streams by role, allocate buffers, queue requests, handle completions |
| Concurrency | Async-first, runtime-agnostic futures; blocking wrappers | Signals/callbacks on libcamera's thread |
| Controls | Typed (exposure a duration, gain a ratio); a frame-accurate schedule per sensor; every frame reports what produced it, read back from embedded data where the sensor has it | Per request; applied by the pipeline handler; metadata per request |
| 3A | In-process Rust (`styx-algo`, ported from the Raspberry Pi IPA, same tuning files); deterministic and replayable bit for bit | IPA module per platform, in-process if signed, else in a sandboxed proxy process |
| Sensors | Data: a TOML description per sensor driven through one generic kernel module, or the sensor's own kernel driver plus a small data file | A kernel driver per sensor meeting libcamera's requirements, plus a `CamHelper` in C++ |
| ISP | PiSP (Pi 5 / CM5) driven from Rust, or a software ISP (fp16 NEON) anywhere | Pipeline handler per platform (PiSP via libpisp, software ISP in the simple pipeline) |
| Frame rate | Any rate the sensor timing allows, exact (frame length per rate) | `FrameDurationLimits` |
| Several consumers | Planned shared captures, both ISP outputs, per-consumer queues; camera service across processes | Several streams per configuration in one process |
| Memory | Frames are leases on camera / ISP buffers (dma-bufs), exported to other processes without copies; a buffer a consumer holds is never rewritten | Request buffers you allocate and recycle |
| Recording | Lossless MCAP recordings (Foxglove, ROS 2 tools) replayed as a camera | Not part of libcamera |
| USB cameras | Its own V4L2 backend (UVC), MJPEG/YUYV decoders and converters | UVC pipeline handler |

## Measured

All on the CM5 dev box (OV9782 1280x800, NV12 through the PiSP) unless stated; libcamera is
v0.6.0 on the same device with the same tuning file (HeliOS `ov9782.json`), through Styx's
libcamera backend (`tools/compare`, one process per path) or the installed HeliOS service.

Fresh side by side (2026-10-02, device clock Aug 17): `tools/compare` (`styx-compare run
--format NV12 --fps 30 --frames 300`, one process per run, three runs each, cold: nothing
remembered between processes), native first, then `camera-mode.sh libcamera` (reboot, `ov9282`
driver) for libcamera, minutes apart in the same dim room (a ceiling, a warm lamp, a blue LED):

| | Styx native (bridge, PiSP, 3A in Rust) | libcamera 0.6.0 (`ov9282`, PiSP, Raspberry Pi IPA) |
|---|---|---|
| `start()` returns | 23.3-23.7 ms | 88.2-93.1 ms |
| open → first frame | 33.7-34.0 ms | 98.8-103.7 ms |
| open → exposure settled (exposure × gain within 5% from then on) | 201.4-201.7 ms | 365.4-370.3 ms |
| open → AE converged (`AeState`) | not reported: AE ends at its limits (33.2 ms × 10) and does not call that converged | 498.6-503.5 ms |
| final exposure × gain; output mean level | 332 ms; 0.317-0.319 | 331 ms; 0.314-0.316 |
| measured rate; interval sd; jitter p99 | 30.000 fps (-0.001%); 0.6-1.0 µs; 2-4 µs | 30.000 fps (-0.001%); 0.9-1.2 µs; 3-5 µs |
| dropped frames | 0 of 300 | 0 of 300 |
| CPU, % of one core (process + children) | 1.1 (1.1 + 0) | 2.5-3.0 (1.0-1.3 + 1.5-1.7 in the IPA proxy process) |
| RSS / PSS (process tree) | 27.3 / 25.6 MiB | 39.2-39.9 / 29.9-30.2 MiB |
| dma-bufs held | 13, 21.5 MiB | 40, 17.0 MiB |
| probe (all backends, libcamera compiled in) | 179-181 ms | 137-139 ms |

The raw results are `target/device-out*/cmp/*.json` of the run; the tool also writes a
markdown table per run (`styx-compare report` merges them).

Earlier measurements (same device, a brighter scene; sources in the last column):

| | Styx native | libcamera | Source |
|---|---|---|---|
| `start()` returns / open → first frame | 45.2-45.4 / 56.4-56.5 ms (3 cold processes) | 119.6-131.3 / 130.6-142.1 ms | [pipeline.md](native-stack/pipeline.md#start-up-and-convergence-2026-10-02) |
| open → AE converged | 256.7-256.8 ms | 663.8-675.3 ms | same |
| open → exposure settled | 223.4-290.0 ms | 530.4-541.9 ms | same |
| restart, camera kept open: stop → first frame / AE locked | 42 ms / frame 2 (109 ms) | — | same |
| CPU per frame, NV12 1280x800 at 30 fps, one consumer | 0.26 ms (0.8% of a core) | — | [pipeline.md](native-stack/pipeline.md#pisp-path-performance) |
| frame start → frame in the consumer's hands | 8.3 ms (9.9 ms with temporal denoise) | 31.9 ms through the HeliOS service (preview encode included) | pipeline.md, [helios-trial.md](native-stack/helios-trial.md) |
| HeliOS service, capture only (preview off): CPU, RSS / PSS | 0.62 ms per frame (60 fps), 28.0 / 26.4 MiB | 0.80 ms per frame (30 fps), 37.1-37.5 / 31.7-32.2 MiB | helios-trial.md |
| image quality, same exposure and tuning: luma shading, R/G, B/G per zone, tone, flat-area noise | within about twice libcamera's own session-to-session difference; noise 0.22-0.23 vs 0.12-0.28 (0..255) | (reference) | [pipeline.md](native-stack/pipeline.md#quality-vs-libcamera) |
| AWB in the same scene | 2543 K / 2643 K | 2488-2491 K / 2596-2602 K | same |
| sensor under its kernel driver (`ov9282`) instead of the bridge: open → first frame, AE locked | 37.5-39.8 ms, 237.6-243.6 ms | — | [adding-a-camera.md](native-stack/adding-a-camera.md#the-ov9782-on-the-cm5-kernel-driver-against-the-bridge) |
| software ISP instead of the PiSP (no ISP hardware), 30 fps | 3.1-3.4 ms CPU per frame on one A76 core (9.4-10.2%) | the simple pipeline's software ISP was not measured here | pipeline.md |
| GStreamer, YUYV 640x480 → RGB, x86 (v4l2loopback) | `styxsrc ! video/x-raw,format=RGB`: 0.7-0.8% CPU | `v4l2src ! videoconvert`: 2.9-3.4% | [ecosystem.md](ecosystem.md) |

What is not comparable:

* **Different sensor drivers and registers.** libcamera drives the OV9782 through the
  `ov9282` kernel driver; Styx through the bridge with the HeliOS register set, which gives
  12-15% more signal at the same exposure and gain code (adding-a-camera.md). Styx under the
  same kernel driver is the closer comparison for start-up (its first frame is 8 ms later than
  through the bridge).
* **Processes.** On the HeliOS image libcamera runs the Raspberry Pi IPA isolated in a proxy
  process (its modules are not signed there); `tools/compare` counts it (the "children" CPU).
  A build with signed IPA modules runs them in-process and saves that process's 1.5% and
  part of its memory.
* **AE definitions.** "Converged" is libcamera's `AeState` and Styx's `AE_STATE` (the same
  values), but each stack decides when it is locked. Styx's AE does not report converged while
  its target is out of reach (exposure and gain at their limits in a dark room); "exposure
  settled" (exposure × gain within 5% from then on) is measured the same way for both.
* **Through Styx.** The libcamera figures are libcamera through Styx's libcamera backend (a
  thin layer: requests recycled on libcamera's thread, frames leased from its buffers), not a
  C++ application; start-up and AE times are libcamera's own, CPU and memory include the
  backend's few threads.
* **Probe.** Both probes enumerate every backend compiled in (V4L2, native, libcamera's camera
  manager start); the native probe also reads each sensor's description and modes.
* **Scenes.** Rows from different days had different light; compare within a table.

## What Styx does not do yet

From [TODO.md](../TODO.md) and what these examples found:

* **Platforms**: the native ISP path is the Raspberry Pi PiSP (Pi 5 / CM5) or the software ISP;
  no Rockchip, Intel IPU, NXP or other hardware ISPs, no GPU ISP. libcamera supports several of
  these.
* **Sensors**: the OV9782 is verified (bridge and kernel driver); the IMX219, IMX477, IMX708
  and OV5647 data files are unverified on hardware; one bridged sensor so far. libcamera has
  tuned helpers for many sensors.
* **Tuning**: the OV9782 uses the HeliOS (libcamera) tuning; Styx has no tuning tool of its own
  (libcamera's Raspberry Pi tuning files are read as they are).
* **Image quality**: black level at high gain differs from libcamera's by ~2.4 codes (10-bit)
  at 8x, unconfirmed; AE at 120 fps can chase 100 Hz flicker (anti-flicker in progress).
* **Controls**: no autofocus, no lens/VCM control, no HDR modes; on processed captures the
  frame rate is fixed per capture (restart for another); ISP settings (denoise) are per Styx
  instance, not per capture.
* **Recordings**: MCAP recordings keep pixels, timestamps and sequence numbers, not a native
  frame's exposure and gains yet (`styx-pipeline`'s raw recordings do).
* **Ecosystem**: the GStreamer and PipeWire bridges exist but have not run on the CM5 (its
  image has neither); the PipeWire node copies each frame once.
* **Robustness**: a real CSI unplug, and runs longer than 2 h, are untested.

## When libcamera is the better choice

* Hardware other than a Raspberry Pi 5 / CM5 with an ISP that libcamera supports (Rockchip
  RK3399, Intel IPU3/IPU6, i.MX8, ...), or a sensor with a tuned libcamera helper and no Styx
  description or data file.
* Applications built on the libcamera ecosystem: `rpicam-apps`, Picamera2, the PipeWire camera
  portal in desktop browsers, Android's camera HAL.
* Features Styx lacks: autofocus, HDR, mature multi-sensor support.

Styx can still sit on top: its optional `libcamera` backend gives libcamera cameras the same
planner, sharing, recording and async API (`--features libcamera`), and probes them next to
native and V4L2 cameras.

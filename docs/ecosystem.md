# Styx Cameras in Ordinary Apps: GStreamer and PipeWire

Two optional bridges let apps that know nothing about Styx use Styx cameras: a GStreamer source
element and a PipeWire camera node. They link C libraries, so they are separate crates with
their own Cargo workspaces, outside the Styx workspace. `cargo build --workspace` at the root
never needs GStreamer or PipeWire installed.

| Crate | What | Needs (Fedora / Nobara) |
|---|---|---|
| `crates/gst-styx` | GStreamer plugin `libgststyx.so`: `styxsrc` and `styxdeviceprovider` | `gstreamer1-devel gstreamer1-plugins-base-devel` |
| `crates/pipewire-styx` | `styx-pipewire`: publishes cameras as PipeWire video sources | `pipewire-devel`, plus libclang for bindgen (`clang-libs`) |

Both build into the repository's `target/` (each crate has `.cargo/config.toml` for this):

```bash
cd crates/gst-styx && cargo build --release        # target/release/libgststyx.so
cd crates/pipewire-styx && cargo build --release   # target/release/styx-pipewire
```

The camera backends follow the `styx` features: `v4l2` (default), `native` (sensors behind the
Styx sensor bridge, e.g. the CM5's OV9782), `libcamera`. `turbojpeg` speeds up MJPEG cameras;
`ffmpeg` (gst-styx) enables FFmpeg/hardware decoders and the H.264/H.265/MJPEG encoders.

## GStreamer: `styxsrc`

```bash
export GST_PLUGIN_PATH=$PWD/target/release
gst-inspect-1.0 styxsrc

# First camera, its own format at its largest size
gst-launch-1.0 styxsrc ! videoconvert ! autovideosink

# A camera by name, part of it, identity key or node; downstream picks the format
gst-launch-1.0 styxsrc camera=/dev/video0 ! video/x-raw,format=RGB,width=640,height=480 ! fakesink
gst-launch-1.0 styxsrc camera=c270 ! video/x-raw,format=GRAY8 ! videoconvert ! pngenc snapshot=true ! filesink location=frame.png

# Camera controls, by V4L2 name with v4l2src's spelling (settable while playing)
gst-launch-1.0 styxsrc extra-controls="c,brightness=10,exposure_time_absolute=100" ! videoconvert ! autovideosink

# Frames from a Styx camera service in another process (shared with its other clients)
gst-launch-1.0 styxsrc service=/run/styx/front.sock camera=ov9782 ! videoconvert ! autovideosink

# Recording
gst-launch-1.0 -e styxsrc ! videoconvert ! x264enc tune=zerolatency ! mp4mux ! filesink location=out.mp4
```

How it works:

- **Caps.** When the element goes to READY it opens the camera's description (probing, like
  `v4l2src`) and lists every mode's size in every format the Styx planner can deliver there: the
  camera's own formats first (largest first), then formats the planner converts to (MJPEG
  decoded, YUYV/NV12 to RGB or luma, Bayer through the software or Pi ISP, encoders when built
  with `ffmpeg`), then `video/x-raw(memory:DMABuf), format=DMA_DRM` variants of the camera's
  own formats. Raw formats: YUY2, UYVY, YVYU, NV12, NV21, I420, YV12, GRAY8, RGB, BGR, RGBA,
  BGRA; also `image/jpeg`, `video/x-h264`, `video/x-h265` (byte-stream, AU).
- **Planning.** Negotiated caps become `FrameRequirements::formats([fourcc])` pinned to the
  negotiated size, with the rate as `min_fps`; the planner picks mode and route, and the element
  runs at the negotiated rate when the mode lists it. The `plan` property shows what was chosen.
  `priority` (latency, throughput, power) and `queue-depth` are passed to the planner.
- **Zero copy.** Frames are wrapped, not copied: camera buffers (V4L2 mmap, libcamera, native)
  and decoder output become `GstMemory` that keeps the Styx frame (and its camera buffer) until
  GStreamer frees it, one memory per plane with a `GstVideoMeta` for strides and offsets. With
  dma-buf caps, the frame's dma-bufs become `GstDmaBufMemory` (linear modifier). If the camera
  cannot export its buffers (e.g. v4l2loopback), the element drops the dma-buf caps and
  renegotiates; `export-dmabuf=true` gives dma-buf memory under system-memory caps where
  possible.
- **Timestamps.** Live source in TIME format. A buffer's PTS is the pipeline clock's running
  time minus the frame's age (from its timestamp clock, else from when the backend received it),
  so frames that waited in Styx's queue keep their capture time and a syncing sink keeps the
  camera's rate. Duration is one frame period; the latency query reports one frame (minimum).
- **Renegotiation.** A reconfigure (e.g. a capsfilter changing) restarts the plan for the new
  caps; frames in the new format follow.
- **Errors.** Unknown camera: error on NULL→READY. No frames for `timeout` ms (default 5000;
  0 waits forever): `ResourceError::Read` ("Was the camera unplugged?"). Capture ended
  (unplugged, service gone): `ResourceError::NotFound`. A failed decode: `StreamError::Failed`.
  `num-buffers` ends with EOS as with any `GstBaseSrc`.
- **Tests:** `camera=virtual` (or `virtual:320x240:YUYV`) is Styx's synthetic camera;
  `cargo test` in `crates/gst-styx` runs real pipelines with it.

### Device monitor

`styxdeviceprovider` lists Styx cameras in `GstDeviceMonitor` (`gst-device-monitor-1.0
Video/Source`, from `gstreamer1-plugins-base-tools`), with their caps; `create_element` gives a
`styxsrc` for that camera (selected by node path, else name). It ranks SECONDARY, like the
element, so `autovideosrc` keeps preferring `v4l2src`, and a USB camera is listed by both the
V4L2 and the Styx provider.

### Measured (x86-64 desktop, v4l2loopback YUYV 640x480 at 30 fps)

Process CPU for 600 frames (20 s), release build, two runs each:

| Pipeline | CPU |
|---|---|
| `v4l2src ! video/x-raw,format=YUY2 ! fakesink` | 0.5%, 0.5% |
| `styxsrc ! video/x-raw,format=YUY2 ! fakesink` | 0.2%, 0.3% |
| `v4l2src ! YUY2 ! videoconvert ! video/x-raw,format=RGB ! fakesink` | 2.9%, 3.4% |
| `styxsrc ! video/x-raw,format=RGB ! fakesink` (Styx converts) | 0.7%, 0.8% |
| `styxsrc ! YUY2 ! videoconvert ! video/x-raw,format=RGB ! fakesink` | 3.2%, 3.0% |

PTS spacing with `fakesink sync=true`: 33.4 ms (virtual camera, loopback), as with `v4l2src`.
Unplugging (stopping the loopback's writer) ends the pipeline with the timeout error 2 s later.
The loopback node cannot export dma-bufs, so the dma-buf path was tested with memfd-backed
frames in unit tests (GstDmaBufMemory per plane, offsets, strides, contents) and not yet with
a real camera.

## PipeWire

Two ways, both giving browsers (through the camera portal), OBS and other PipeWire clients a
`Video/Source` node with `media.role = Camera`:

### `styx-pipewire` (native node, preferred)

```bash
styx-pipewire                                  # every camera Styx finds
styx-pipewire --camera c270 --camera ov9782    # selected cameras
styx-pipewire --service /run/styx/front.sock   # cameras of a Styx camera service
```

Each camera becomes a PipeWire stream node `styx.<name>` (`node.description` "<name> (Styx)").
Its `EnumFormat` params are what the planner can deliver (YUY2, UYVY, NV12, I420, RGB, BGR, RGBA,
BGRA, GRAY8 at every mode's size and rates). When a consumer picks a format the node plans and
starts the capture on its own thread; when the consumer pauses or leaves, the camera stops. The
node drives the graph (`DRIVER`) and triggers a cycle for every camera frame; each frame is
copied once into PipeWire's buffer (packed rows), which is what PipeWire's memfd buffers need.
With `--service`, the camera service owns the camera and plans for all its clients;
the node offers common sizes and drops frames of another size than negotiated.

Status: written against `pipewire` 0.10 / `libspa` 0.10, **not built or run yet** on the
development host (no `pipewire-devel`). Its format and capture logic builds and tests without
PipeWire: `cargo test --no-default-features`. Zero-copy (`SPA_DATA_DmaBuf` buffers from camera
dma-bufs) is a possible next step.

### `styxsrc ! pipewiresink` (works today)

```bash
gst-launch-1.0 styxsrc camera=c270 ! videoconvert ! \
  pipewiresink mode=provide client-name=styx-c270 \
  stream-properties="props,media.class=Video/Source,media.role=Camera,node.description=C270"
```

Tested on the development host (PipeWire 1.6.8): the node appeared with `media.class =
Video/Source`, `media.role = Camera`, and `pipewiresrc target-object=styx-c270` received 60
frames in 2 s (YUY2 640x480 at 30 fps). This costs a GStreamer pipeline per camera and copies
in `pipewiresink`, and the camera runs while the pipeline does, not only while consumers
stream.

## Limitations

- Controls: `extra-controls` sets controls by name for in-process cameras; there are no
  per-control properties (they differ per camera), and controls are not available through a
  camera service. PipeWire nodes expose no controls.
- `styxsrc` caps list each mode's own size; downscaled outputs the planner can make
  (`output_resolution`, ISP scaling, DCT-domain JPEG scaling) are not offered as caps yet.
- Through a camera service, caps are generic ranges; the service plans the size itself (it keeps
  the aspect ratio and never upscales) and the element renegotiates to what arrives.
- dma-buf caps are offered for V4L2, native and libcamera cameras before knowing whether the
  buffers can be exported; an export failure costs one renegotiation.
- The Raspberry Pi CM5 (HeliOS image) has neither GStreamer nor PipeWire, so neither bridge has
  run on the OV9782 yet.

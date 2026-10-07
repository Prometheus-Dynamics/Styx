# Grey Test Recordings (`styx-record`)

`styx-record` (`tools/styx-record`) records what a camera sees as raw grey video (the Y plane,
at the camera's native size) in the layout Eidos reads its robot video in, with a per-frame
sidecar (sensor timestamp, clock, sequence number, dropped frames) and a JSON file of the camera
settings in effect. It is a small binary without codecs, for capturing test recordings on a Raze
device (CM5, OV9782).

It takes frames two ways:

- **From a Styx camera service** (default): a normal `FrameClient` next to the service's other
  clients. It asks for grey frames; the service plans them on the running capture (a luma view
  of NV12, no copy), so joining does not restart the capture for the others when they use the
  camera's full-size mode (the recorder checks the service's restart counter and warns
  otherwise). Controls are read through a `ControlClient`, which never touches the capture.
- **Directly** (`--source direct`): the recorder opens the camera itself through Styx's
  libcamera or native backend, for devices where no Styx camera service runs (the PhotonVision
  image, where PhotonVision owns the camera through libcamera). A camera another process holds
  is refused with a clear message (`camera ... is busy: another process has it open
  (PhotonVision, ...)`); stop that process first.

## Usage

```text
styx-record --camera front --seconds 30 --out /data/rec/x            # every frame, 30 s
styx-record --mode latest --seconds 30 --out /data/rec/x             # the newest frame only
styx-record --direct --camera ov9782 --frames 900 --out /data/rec/y  # no service: libcamera
styx-record --stills 20 --on-key --out /data/calib/a                 # a still per Enter
styx-record --stills 20 --every-secs 2 --out /data/calib/b           # a still every 2 s
```

| Option | Meaning |
|---|---|
| `--source service\|direct`, `--direct` | camera service (default) or the camera itself |
| `--socket PATH` | the service's socket (`$STYX_SOCKET`, else `/tmp/styx-camera.sock`) |
| `--camera NAME` | the camera: its name, part of it, or an identity key (default: the first) |
| `--mode every\|latest` | every frame, queued, with drops counted (default); or the newest frame only |
| `--size WxH`, `--fps N` | frame size (default: the largest mode, the native size); frame rate (direct mode) |
| `--seconds S`, `--frames N` | stop after S seconds or N frames (neither: until Ctrl-C) |
| `--stills N` with `--on-key` or `--every-secs K` | N stills, one per Enter on stdin or every K seconds |
| `--out DIR`, `--name NAME` | output directory (created); file name stem (default: the directory's name) |
| `--queue N` | frames the disk writer may queue (default 32) |
| `--force`, `--quiet` | overwrite existing files; no progress lines |

The `styx record` spelling works too (`styx-record record ...`).

Ctrl-C (SIGINT) or SIGTERM stops the recording cleanly: queued frames are written, the raw file
holds whole frames only, the files are synced, and the settings file is finalised with
`"stop_reason": "interrupted (Ctrl-C / SIGTERM)"`. The process exits 0. Paths of the files are
printed on stdout; progress (every 5 s) and the summary go to stderr.

A still is the first frame exposed after the trigger (Enter, or the timer): frames already
queued before it are skipped.

## Files

For `--out /data/rec/x` (name `x`), a 1280x800 camera:

| File | Contents |
|---|---|
| `x_1280x800_gray.raw` | the video: Y planes, `width*height` bytes each, rows top to bottom without padding, frames back to back, no header |
| `x.frames.csv` | one row per frame of the raw file (or per still) |
| `x.json` | camera settings, controls, changes while recording, what was recorded |
| `x_still000_1280x800.gray.raw`, `.pgm`, `.txt` | stills mode: one set per still, instead of the video |

**The raw video is Eidos's robot video layout.** Eidos's pinned robot video is
`recording_7d7ca24f_1280x800_gray.raw` (Eidos `fixtures/benchmarks/recording_7d7ca24f.json`:
`raw_gray_bytes` 2091008000 = 2042 frames x 1280 x 800, no header), read by
`eidos_detect_raw_stream --input FILE --width W --height H` (Eidos
`examples/eidos-detectors/detect_raw_stream.rs`, which reads `width*height` bytes per frame and
refuses a partial frame at the end) through `scripts/cm5-video-secondary-test.sh`. The size is
in the name and in the settings file, not in the file.

**Stills are Eidos's live-capture layout** (Eidos
`crates/benches/src/bin/styx_aruco16h5_realtime/overlay.rs`, `testing/aruco16h5_webcam_cases`):
`<stem>.gray.raw` (one Y plane), `<stem>.pgm` (P5, the same bytes) and `<stem>.txt` with
`key=value` lines: `width`, `height`, `format=R8`, `source_fourcc`, `frame_index` (the
sequence number), `capture_index` (the still's number), `raw`, `pgm`, plus `sequence`,
`timestamp_ns`, `clock`, `camera`, `exposure_us`, `analogue_gain`, `styx_commit`.
`eidos_detect_image --input x_still000_1280x800.gray.raw --raw-luma 1280x800` reads them.

### `x.frames.csv`

```text
frame,sequence,timestamp_ns,clock,dropped_before,gap_source,received_monotonic_ns,exposure_us,analogue_gain,frame_duration_us,file
0,1200,123456789000,boottime,0,first,123456797000,,,,
1,1201,123490122000,boottime,0,sequence,123490130000,,,,
2,1204,123590122000,boottime,2,sequence,123590131000,,,,
```

| Column | Meaning |
|---|---|
| `frame` | row number: the frame's index in the raw file (or the still's number) |
| `sequence` | the camera's frame sequence number (empty when the camera has none) |
| `timestamp_ns` | the sensor timestamp (start of exposure where the backend says so), ns on `clock` |
| `clock` | `boottime` (libcamera), `monotonic` (V4L2, native), `realtime`, `stream` (replays), or empty |
| `dropped_before` | camera frames missing between the previous row and this one |
| `gap_source` | how: `sequence` (exact), `timestamp` (no sequence numbers: from the timestamps and the frame interval), `reset` (the sequence went backwards: the capture restarted), `first`, `unknown`, `still` |
| `received_monotonic_ns` | when the recorder received the frame, `CLOCK_MONOTONIC` |
| `exposure_us`, `analogue_gain`, `frame_duration_us` | the frame's own values, where frames carry them (a sensor Styx drives, opened directly); empty otherwise |
| `file` | stills: the still's `.gray.raw` |

Drops are every frame the recording does not have, wherever it was lost: in the camera, in the
service (a client too slow for its queue), or by the recorder when the disk did not keep up.
With `--mode latest` gaps are expected (only the newest frame is taken).

Through a camera service, frames carry the sequence number (the service sends it with each
frame); the per-frame exposure and gain do not cross the socket, so the settings file has them
from the controls instead.

### `x.json`

```json
{
  "recorder": "styx-record",
  "settings_version": 1,
  "styx_version": "2.0.0",
  "styx_commit": "<git commit the binary was built from>",
  "start_unix_ns": 1791290096789000000,
  "start_utc": "2026-10-06T12:34:56.789Z",
  "end_unix_ns": ..., "end_utc": "...",
  "complete": true,
  "stop_reason": "duration reached",
  "mode": "every",
  "layout": "raw grey: the Y plane, ...",
  "format": "R8",
  "width": 1280, "height": 800,
  "frame_format": "GREY",
  "frames": 900,
  "raw_file": "x_1280x800_gray.raw",
  "raw_gray_bytes": 921600000,
  "raw_gray_sha256": "...",
  "stills": [],
  "frames_csv": "x.frames.csv",
  "camera": {"name": "...", "keys": [...], "backend": "libcamera", "source": "direct", "socket": null, "plan": "..."},
  "settings": {
    "exposure_us": 10000, "analogue_gain": 2, "fps": 30, "frame_duration_us": 33333.3,
    "ae_enable": true, "awb_enable": true, "exposure_value": 0,
    "colour_temperature_k": null, "red_gain": null, "blue_gain": null,
    "values_from": "camera controls"
  },
  "controls": [{"name": "ExposureTime", "id": 1, "value": 10000, "standard": null}, ...],
  "changes": [{"frame": 412, "at_ms": 13750.2, "name": "ExposureTime", "value": 12000, "from": "control"}],
  "timestamp_clock": "boottime",
  "first_timestamp_ns": ..., "last_timestamp_ns": ...,
  "dropped_frames": 3, "drop_gaps": 2,
  "writer_dropped_frames": 0, "writer_max_queued": 2,
  "service_restarts_caused": 0,
  "errors": []
}
```

- `settings`: what was in effect at the start. Exposure, gain and frame duration come from the
  frames' own metadata when they carry it, else from the camera's controls (Styx's standard
  controls through a service, libcamera's `ExposureTime`, `AnalogueGain`, `AeEnable`,
  `AwbEnable`, ... directly); `fps` from the plan the capture runs.
- `controls`: every control and its value at the start. The controls are read again every
  second while recording, and each value that changed is listed in `changes` with the frame it
  was seen at (`from: "control"`); changes in the frames' own exposure, gain and frame duration
  too (`from: "frame"`; the CSV has every frame's).
- `width`, `height`, `frames`, `raw_gray_bytes`, `raw_gray_sha256` are the fields of an Eidos
  fixture descriptor (`fixtures/benchmarks/*.json`), so a recording can be pinned as one.
- `complete` is false only when writing failed (disk full, I/O error); the raw file then holds
  the whole frames written before the failure, and `errors` says what happened.
- `service_restarts_caused`: how many times the service restarted its capture while the
  recorder joined (0: the other clients were not interrupted).
- The file is written when recording starts (`"stop_reason": "recording"`, `complete` false) and
  replaced when it ends, so an interrupted recording that could not finalise still has one.

## Disk writes

The disk writer runs on its own thread behind a bounded queue (`--queue`, 32 frames): the
recorder copies each frame's Y plane into one of the writer's buffers and releases the frame at
once, so a slow disk never holds camera buffers the service wants back. When the queue is full
the frame is dropped, counted (`writer_dropped_frames`) and shows up as a drop in the CSV; in
every-frame mode the recorder prints `the disk is not keeping up` at the first one and the
summary says so. At 1280x800 and 30 fps the video is 30.7 MB/s.

## Building

```sh
cargo build --release -p styx-record-cli                         # service client only
cargo build --release -p styx-record-cli --features libcamera    # + direct mode (libcamera)
cargo build --release -p styx-record-cli --features native       # + direct mode (native stack)
```

Without features the binary is a camera service client only: it links neither libcamera nor
libstdc++, only glibc and libgcc_s (HeliOS, with or without libcamera). `libcamera` adds
direct mode through libcamera and links `libcamera.so`, `libcamera-base.so` and libstdc++ (the
PhotonVision image has them); `native` adds the native stack (sensors Styx drives), `v4l2` USB
cameras. The commit in the settings file is taken from `git` at build time, or from
`$STYX_COMMIT` when building outside a checkout.

### Buildroot / Gaia packaging

Both Raze images are Buildroot-based (aarch64, glibc), so the binary is cross-built against the
image's sysroot and installed as `/usr/bin/styx-record`:

- **HeliOS**: build without features (a Styx camera service runs there); package it as a
  `cargo`-built Buildroot package in the device's `buildroot-external` (or next to the
  `helios-peripherals` package that builds Styx), `STYX_COMMIT` set from the pinned Styx
  revision, installed to `$(TARGET_DIR)/usr/bin/styx-record`. Release builds as gaia builds them
  (`opt-level = "z"`, fat LTO) are smaller still.
- **PhotonVision (Raze) image**: build with `--features libcamera` against the image's
  `buildroot-output/staging` sysroot (libcamera 0.7) with the image's toolchain; bindgen needs
  the toolchain's libstdc++ headers (`BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu`, as in
  the CM5 prep's `build-aarch64.sh`). Run it with PhotonVision stopped (it owns the camera), or
  record the frames PhotonVision publishes instead.

No configuration files, services or data are needed; the binary is self-contained apart from
glibc and libgcc_s (and libcamera and libstdc++ with that feature).

## Tests

`tools/styx-record/tests/record.rs` runs the recorder against cameras served by a Styx camera
service in the test process: a replayed NV12 camera with known Y planes and sequence gaps
(byte-exact raw file, CSV rows and gaps, settings file), a virtual camera with controls next to
another client (no restart, the other client keeps its frames, a control change recorded),
stills on key and on a timer, direct mode with per-frame exposure, a "disk" (a pipe) that does
not keep up, and Ctrl-C on the real binary (`SIGINT`, files finalised).

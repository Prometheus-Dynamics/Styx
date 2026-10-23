# Simulated cameras

Styx can run a camera without the device: a **simulated sensor** behind the same native camera,
session, sensor driver, ISP loop and camera service as on the CM5. It is for tier 1 of the fast
test pipeline (a Docker sim of the OS, CI, cloud agents): everything above the sensor runs as on
the device, and the sensor responds as the real one does.

Build with feature `sim` (it includes `native`; add `replay-mcap` or `replay-styxrec` to play
recordings). It builds and runs on x86_64 without a PiSP or any camera hardware.

## Selecting simulated cameras

Simulated cameras are listed by `styx::probe_all()` next to (or instead of) real ones, so a
program that discovers cameras and serves them, such as HeliOS's peripherals, needs no change:

- `STYX_SIM_CAMERAS`: one spec per entry, `;` or new lines between entries; or, when it is not
  set,
- `/etc/styx/sim-cameras`: one spec per line, `#` comments.

From a program: `styx::sim::device(SimSpec)` or `styx::sim::device_from("ov9782,scene=chart")`
registers one for the process and returns its probed device.

Each camera is keyed `sim:<sensor>:<n>` (`n` counts the cameras of that sensor from 0) and named
`<sensor> (simulated sim:<sensor>:<n>)`.

### Spec

`<sensor>[,key=value]...`. The sensor is a description's name, found as for real cameras:
`STYX_SENSOR_PATH`, `~/.config/styx/sensors`, `/etc/styx/sensors`, `/usr/local/share/styx/sensors`,
`/usr/share/styx/sensors`, then the built-in ones (`ov9782`).

| key | meaning | default |
|---|---|---|
| `scene` | `chart` (grey ramp, colour checker, fine checkerboard, a moving bright square), `flat`, or `tags` (AprilTags in 3D, [below](#tags)) | `chart` |
| `light` | scene brightness (1: mid grey at 18% of full scale at 10 ms, gain 1) | 1 |
| `grey` | `flat`'s reflectance | 0.18 |
| `recording` | a raw recording (MCAP or styxrec of Bayer frames, e.g. `Frames::sensor_raw()`), looped | |
| `noise` | sensor noise `on`/`off` | on |
| `drop` | probability a frame is lost on the link (the sequence skips it) | 0 |
| `corrupt` | probability a frame arrives flagged corrupted | 0 |
| `jitter_ms` | frame start jitter, up to this early or late | 0 |
| `fail_every_s` | a capture error (the queue fails; the capture restarts) every this long | off |
| `unplug_every_s` | the sensor disappears every this long while streaming ... | off |
| `unplug_for_s` | ... for this long (not listed, does not open), then comes back | 2 |
| `seed` | seed of the faults and the noise | 0 |
| `truth` | where each frame's ground truth goes (JSON Lines; scenes that know it: `tags`) | |

Examples:

```text
ov9782
ov9782,scene=chart,light=0.5
ov9782,recording=/data/clips/field.mcap
ov9782,drop=0.01,jitter_ms=1.5,fail_every_s=60,unplug_every_s=300,unplug_for_s=5
```

## Tags

`scene=tags` renders AprilTag 36h11 tags at their poses in 3D, seen through the camera's lens
from its pose or along a trajectory, so tag detection, multi-tag pose and mapping run in the
sim. Each pixel's ray comes from the calibration's lens model (the equidistant fisheye or the
pinhole with radial and tangential distortion, as OpenCV and HeliOS define them); inside each
tag's projected bounds it meets the tag's plane and reads the tag's cell (2x2 samples per
pixel); elsewhere it meets a floor (z = 0, a 1 m checker) or a plain wall. With a moving camera
the tags blur over the exposure (one pose per 2 ms, up to 8).

| key | meaning | default |
|---|---|---|
| `layout` | the tags: a WPILib AprilTag field layout (`.json`: `tags` with `ID` and `pose`) or a Limelight map (`.fmap`: `fiducials` with `id`, `size` in mm, a row-major 4x4 `transform`) | four tags (ids 1 to 4) on a wall 3 m ahead |
| `tag_size` | the black square's side (m), for WPILib layouts | 0.1651 (FRC) |
| `calibration` | the camera's calibration, HeliOS's JSON: `width`, `height`, `model` (`fisheye` or `pinhole`), `fx`, `fy`, `cx`, `cy`, `distortion` (`k1`..`k6`, `p1`, `p2`) | a fisheye, f = 560 px at 1280x800 |
| `camera` | a JSON file: `{"pose": {...}}` or `{"trajectory": [{"t": 0, ...}, ...]}` (looped), each pose `x`, `y`, `z` (m) and `roll`, `pitch`, `yaw` (degrees) | |
| `pose` | the camera's pose inline: `x:y:z:roll:pitch:yaw` | `0:0:0.5:0:0:0` |
| `light` | brightness (1: white paper at 80% of full scale at 10 ms, gain 1) | 1 |
| `antialias`, `blur` | `on` or `off` | on |

Frames are WPILib's: X forward, Y left, Z up; a rotation is roll about X, then pitch about Y,
then yaw about Z, about the fixed axes. The camera looks along its +X. A tag faces its +X; seen
from the front, its +Y is to the right and +Z up. The calibration covers the whole pixel array
at its size; a mode of another size or field (a binned or cropped mode) is mapped onto it.

```text
ov9782,scene=tags,layout=/usr/share/helios/fields/2025-reefscape.json,calibration=/data/cal/cam0.json,camera=/data/sim/drive.json,truth=/tmp/truth.jsonl
```

### Ground truth

With `truth=<path>`, each frame appends one line of JSON: `sequence` (the frame's
`NativeFrameMeta::sequence`; it starts at 0 with each stream), `timestamp_ns` (the frame's start,
`CLOCK_MONOTONIC`), `width`, `height`, `hflip`, `vflip`, `camera` (`position` m and
`orientation_wxyz`, the camera body in the layout's frame at the middle of the exposure) and
`tags`: each visible tag's `id` and `corners`, the black square's top left, top right, bottom
right and bottom left as printed, in pixels of the delivered frame (flips applied; pixel centres
at integers, as OpenCV's). A tag is listed when all four corners are in the frame.

Checked with OpenCV 4.13's AprilTag 36h11 detector (`DICT_APRILTAG_36h11`, sub-pixel corners) on
four views of the default tags at 1280x800: every tag found with its id, corners within 0.4 to
1.4 px of the truth on tags about 30 px across. Rendering a moving camera's frame with blur
takes about 15 ms at 1280x800 (release, the dev PC); a still camera's frame is made once.

## What is simulated, and how faithfully

| | how | as on the device |
|---|---|---|
| sensor registers | the driver writes them over a simulated bus | the description's mode, format, init, frame length, exposure, gain and flip registers |
| controls | the real control schedule | ranges, clamping (exposure held to the frame length), delays and landing frames, frame rate limits per mode |
| modes | the description's | the same list, binning and crop (`CameraInfo::modes`, `sensor_mode`) |
| frames | rendered per frame from the scene at the exposure and gain the schedule applied to that frame | black level, clipping, shot and read noise, the receiver's format (CSI-2 packed RAW10 for the OV9782) |
| flips | the array is read out as the flip registers say | the picture mirrors or turns, the Bayer order and format change (`pBAA` → `pRAA`), the ISP follows |
| timing | a frame starts every frame period (the applied frame length) | monotonic timestamps at frame start, frame-start events |
| bridge | the bridge's start and stop requests with acknowledgements | the session's start, stop and restart paths |
| ISP | the software ISP with the 3A loop and the sensor's tuning | raw frames for `Frames::sensor_raw()` consumers (`native_sensor_raw`), 16-bit Bayer as the PiSP gives them (a copy here); PiSP-only features do not run: sliced delivery, lazy back end buffers, the second output; embedded data is not sent |

Rendering a 1280x800 frame takes about 4.6 ms on one x86 core (7.3 ms with noise; release
build, 2026-10-23, the dev PC), so a simulated camera keeps up with 120 fps; the software ISP
after it costs more than the sensor.

A recording is played one recorded frame per sensor frame, with each recorded frame's exposure,
gain and black level taken out, so AE and manual exposure act on it as on a live scene. A
recording of another size is scaled to the mode.

## Faults

Faults come from the spec. `drop`, `corrupt` and `jitter_ms` act on single frames; a capture
error (`fail_every_s`) ends the capture with an error, and a supervised capture (every planned
capture and camera service) restarts it. `unplug_every_s` takes the sensor away: the capture
ends, the camera is not listed and does not open until it comes back, as a sensor that drops off
the bus.

## Packaging

`scripts/install-sensors.sh DESTDIR` installs one description per sensor at
`$DESTDIR/usr/share/styx/sensors/<sensor>.toml` (what Gaia's hardware check expects), and
`$DESTDIR/etc/styx/sim-cameras` with a default simulated camera when given `--sim`.

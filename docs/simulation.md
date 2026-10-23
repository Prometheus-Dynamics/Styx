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
| `scene` | `chart` (grey ramp, colour checker, fine checkerboard, a moving bright square), `flat`, or `gltf` (a glTF scene in 3D, [below](#gltf-scenes)) | `chart` |
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
| `truth` | where each frame's ground truth goes (JSON Lines; scenes that know it: `gltf`) | |
| `clock` | `real`, or `lockstep` ([below](#lockstep-time)) | `real` |

Examples:

```text
ov9782
ov9782,scene=chart,light=0.5
ov9782,recording=/data/clips/field.mcap
ov9782,drop=0.01,jitter_ms=1.5,fail_every_s=60,unplug_every_s=300,unplug_for_s=5
```

## glTF scenes

`scene=gltf,path=<scene.gltf or .glb>` renders a glTF 2.0 scene in 3D, seen through the camera's
lens from the scene's camera node, so detection, pose estimation and mapping run in the sim on
whatever the application puts in the scene. Styx knows nothing about what the scene shows: the
application builds it (from a field map, a test rig, ...) and Styx loads it. The same file
loads in the Bevy simulation backend (`simulation-bevy`), so the GPU and CPU renders can be
compared.

Each pixel's ray comes from the lens model (OpenCV's equidistant fisheye, or the pinhole with
radial and tangential distortion) and meets the nearest triangle; the light it sees is the
material's base colour (factor times texture; sRGB textures decoded, the sampler's filter and
wrap honoured) times the brightness, unlit. With a moving camera the frame blurs over the
exposure (one pose per 2 ms, up to `blur_samples`); a still camera's frame is rendered once.

| key | meaning | default |
|---|---|---|
| `path` | the scene: `.gltf` (with its buffers and images) or `.glb` | required |
| `camera` | the camera node's name | the first node with a camera |
| `lens` | `fisheye` or `pinhole` | `pinhole` |
| `fx`, `fy`, `cx`, `cy` | the lens's focal lengths and principal point (px; `fy` defaults to `fx`) | |
| `lens_width`, `lens_height` | the size the lens keys describe (the whole pixel array) | required with `fx` |
| `k1`..`k6`, `p1`, `p2` | distortion: `k1`..`k4` for the fisheye; `k1`..`k6`, `p1`, `p2` for the pinhole | 0 |
| `light` | brightness (1: a base colour of 0.8 at 80% of full scale at 10 ms, gain 1) | 1 |
| `background` | what a ray that meets nothing sees (reflectance) | 0.3 |
| `antialias`, `blur` | `on` or `off` (antialiasing: 2x2 samples per pixel of a still camera) | on |
| `blur_samples` | poses per frame at most, for blur | 4 |
| `threads` | render threads | the cores, up to 8 |

Without the lens keys, the lens is a pinhole without distortion from the glTF camera's
`yfov`; a scene without a perspective camera then needs them. The lens keys describe the whole
pixel array at `lens_width` x `lens_height`; a mode of another size or field (binned, cropped)
is mapped onto it.

What the scene carries, in glTF's conventions (metres, +Y up, right-handed):

- **The camera**: the camera node (or `camera=<name>`) with its ancestors' transforms. It looks
  along its -Z with +Y up, as glTF cameras do. Translation and rotation animations on it or its
  ancestors move it, looped over the longest animation.
- **Ground truth points**: nodes named `truth:<name>` (e.g. `truth:tag7/tl`); the point is the
  node's origin.
- **Materials**: mark them unlit (`KHR_materials_unlit`) so that the Bevy backend, which
  lights the scene, shows the base colour as the simulated cameras do (they ignore lights).

`sim_bevy_cross` (crates/styx/tests/it, features `sim` and `simulation-bevy`, ignored without
a GPU: `--run-ignored all`) renders the test scene with Bevy and checks that the square lands
where the simulated cameras' lens model puts it (edge within a pixel) with its texture the
right way round.

```text
ov9782,scene=gltf,path=/data/sim/field.glb,lens=fisheye,lens_width=1280,lens_height=800,fx=560,fy=560,cx=639.5,cy=399.5,k1=0.03,truth=/tmp/truth.jsonl
```

### Ground truth

With `truth=<path>`, each frame appends one line of JSON: `sequence` (the frame's
`NativeFrameMeta::sequence`; it starts at 0 with each stream), `timestamp_ns` (the frame's start,
`CLOCK_MONOTONIC`), `width`, `height`, `hflip`, `vflip`, `camera` (`position` m and
`orientation_wxyz`, the camera node in the scene's frame at the middle of the exposure) and
`points`: each `truth:` point's `name` (without `truth:`) and `x`, `y` in pixels of the
delivered frame (flips applied; pixel centres at integers, as OpenCV's). A point is listed when
it is in the frame and nothing in the scene is in front of it.

## Lockstep time

With `clock=real` frames come every frame period by the monotonic clock; a scene slower than
the frame rate gives fewer frames, further apart. With `clock=lockstep` each frame's timestamp
and scene time are exactly one frame period (the frame length the control schedule applied)
after the last frame's, however long rendering takes, and a frame waits for a free buffer (up
to 500 ms, then it is dropped as on the device) instead of being dropped at once. A slow scene
then runs slower than real time, but frame N shows the scene at N frame periods, so trackers
see the sensor's true frame spacing.

Lockstep timestamps start at the monotonic clock when the stream starts and never run ahead of
it; frame metadata, frame-start events and the truth file carry the same timestamp. Faults run
on the same clock: jitter moves the timestamp and scene time, a lost frame still takes its
period, and `fail_every_s` and `unplug_every_s` count stream time.

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

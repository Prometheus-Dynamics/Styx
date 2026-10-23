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
| `scene` | `chart` (grey ramp, colour checker, fine checkerboard, a moving bright square) or `flat` | `chart` |
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

Examples:

```text
ov9782
ov9782,scene=chart,light=0.5
ov9782,recording=/data/clips/field.mcap
ov9782,drop=0.01,jitter_ms=1.5,fail_every_s=60,unplug_every_s=300,unplug_for_s=5
```

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

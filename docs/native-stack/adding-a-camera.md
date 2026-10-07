# Adding a camera to the native stack

The native stack drives a CSI-2 sensor in one of two ways, behind one interface
(`styx_native::NativeCamera`, `CameraControls`, the `native` provider and the Styx `native`
backend). Everything above the sensor (the receiver path, buffers, the PiSP and software ISP
paths in `styx-pipeline`, the 3A loop, the planner, the camera service) is the same for both.

| | sensor with a kernel driver | sensor without one |
|---|---|---|
| who talks to the sensor | its upstream (or vendor) kernel driver | `styx-sensor`'s driver over I²C, through the Styx sensor bridge |
| what Styx needs | nothing, or one small data file | a sensor description, the bridge overlay |
| controls | V4L2 controls on the subdevice (`EXPOSURE`, `ANALOGUE_GAIN`, `DIGITAL_GAIN`, `VBLANK`, `HFLIP`/`VFLIP`, `TEST_PATTERN`) | registers (group hold), exactly when the description says |
| timing | `PIXEL_RATE`, `HBLANK`, `VBLANK` as the driver reports them for the mode | the description's timing model |
| values a frame was made with | embedded data if the driver sends it, else the delay model | embedded data if the description has a layout, else the delay model |
| key in listings | `sensor:/dev/v4l-subdevN` | `bridge:/dev/v4l-subdevN` |
| example | the OV9782 under `ov9282`, Raspberry Pi cameras (imx219, imx477, imx708, ov5647) | the OV9782 behind the bridge (`crates/sensor/sensors/ov9782.toml`) |

Where both exist (the same sensor, the bridge overlay applied), the bridge is the one listed:
discovery skips a kernel-driven sensor whose I²C address a bridge has.

## A sensor with a kernel driver

Plug it in (device tree overlay as for libcamera, e.g. `dtoverlay=imx708`). Styx finds it
through the media graph: every entity with function `MEDIA_ENT_F_CAM_SENSOR` that has a
subdevice node and feeds a receiver with a raw video node is a camera
(`styx_native::kernel::discover_kernel`). From its subdevice Styx reads (read-only at
discovery):

* modes: `VIDIOC_SUBDEV_ENUM_MBUS_CODE` × `ENUM_FRAME_SIZE` (one mode per size and bit depth),
  `NATIVE_SIZE` and `CROP_BOUNDS` selections;
* timing: `PIXEL_RATE`, `HBLANK`, `VBLANK` (range and value) give the frame rate range,
  `fps = pixel_rate / ((width + hblank) × (height + vblank))`;
* controls: `EXPOSURE` (lines; its maximum at the current `VBLANK` gives the exposure
  margin), `ANALOGUE_GAIN` and `DIGITAL_GAIN` code ranges, `HFLIP`/`VFLIP` (and whether they
  change the Bayer order, `V4L2_CTRL_FLAG_MODIFY_LAYOUT`), the `TEST_PATTERN` menu, the
  `LINK_FREQ` menu.

The driver reports blanking and exposure ranges for the mode that is set, so at discovery
every mode gets the current mode's ranges (frame rate ranges in listings are approximate);
configuring sets the sensor format, re-reads them and rebuilds the description for the mode
(exact from then on).

What a driver does not report comes from a data file, as libcamera keeps it in its
`CamHelper` classes:

```toml
# imx219.kernel.toml
name = "imx219"
drivers = ["imx219"]          # entity names (first word); default: name
verified = false
tuning = "imx219.json"
analog_gain = { reciprocal = { numerator = 256, base = 256 } }   # code -> gain
delays = { exposure = 2, analog_gain = 1, frame_length = 2 }     # frames
black_level = { value = 64, bits = 10 }

[embedded_data]               # only if the driver has a metadata pad
lines = 2
format = "ccs"                # CCS/SMIA tagged register dump
registers = [
    { control = "analog_gain", address = 0x0157 },
    { control = "exposure", address = 0x015a, bytes = 2 },
    { control = "frame_length", address = 0x0160, bytes = 2 },
]
```

All fields but `name` are optional (`KernelSensorData`, `crates/sensor/src/kernel_data.rs`;
`colour = true/false` tells a driver for a family apart by the codes it reports, as `ov9282`
drives the mono OV9281 and the colour OV9782). Styx looks for `*.kernel.toml` in the
description search path (`$STYX_SENSOR_PATH`, `~/.config/styx/sensors`, `/etc/styx/sensors`,
`/usr/{local/,}share/styx/sensors`), then in the files built into `styx-sensor`
(`crates/sensor/sensors/kernel/`):

| sensor | driver entity | data | checked on hardware |
|---|---|---|---|
| OV9782 | `ov9782`, `ov9282` (colour codes) | gain code/16, delays 2/2/1, black 64 (10 bit), one extra line per frame | yes (CM5, below) |
| OV9281 / OV9282 | `ov9281`, `ov9282` (mono codes) | libcamera's helper | no |
| IMX219 | `imx219` | libcamera's helper, CCS embedded data | no |
| IMX477 | `imx477` | libcamera's helper, CCS embedded data | no |
| IMX708 | `imx708` | libcamera's helper, CCS embedded data, PDAF, its DW9817 lens (`dw9807-vcm`) | no |
| OV5647 | `ov5647` | libcamera's helper | no |

Without a data file the camera still works: gain is taken as linear with the driver's
minimum code as 1×, delays as libcamera's defaults for unknown sensors (exposure 2, gain 1,
frame length 2), no black level (the tuning's, if any). A new file is needed only to get
these right; check them with `landing` (below).

What happens when it runs (`crates/native/src/kernel.rs`):

* **open**: the subdevice is opened (an advisory lock makes it exclusive between Styx
  processes; it is busy while libcamera holds the media device's lock, which it takes while
  it has a camera acquired) and the flips are set to the description's defaults (off) before
  anything sets a format.
* **configure**: the sensor format is set, the ranges re-read, the mode's defaults and the
  start frame duration set as controls (`VBLANK`, `EXPOSURE`, `ANALOGUE_GAIN`).
* **start**: values asked for before the stream are set at once (the driver applies what is
  set when it starts), then the receiver's `STREAMON` starts the sensor (`s_stream`) and the
  control schedule runs from frame 0. There is no bridge to serve. With the PiSP front end
  owning the capture nodes (`start_external`), the schedule starts in `resume_external`,
  after the front end's `STREAMON`, so the 3A loop's start values (set in between) are on
  frame 0.
* **per frame**: the receiver's frame-start events (`FRAME_SYNC`) drive the same control
  schedule as on the bridge (libcamera's `DelayedControls`): a value for frame F is set in
  frame F − delay, `VBLANK` in a call of its own before the others (the driver widens the
  exposure range when the frame grows; an exposure in the same call would be clamped to the
  old range first). The 3A loop's requests are set at once when enough of the current frame
  is left (`request_at_now`, 4 ms). Each frame reports the values that produced it: read
  back from embedded data when the driver sends it, predicted from the delays otherwise.
* **stop**: `STREAMOFF`; the driver stops and powers the sensor down (runtime PM).

## A sensor without a kernel driver

Write a description (`crates/sensor` docs, `ov9782.toml` as the example): identity and
chip id, pixel array, power sequence, init and mode registers, formats, exposure, gain and
frame length registers with their models, delays, group hold, flips, embedded data layout.
Put it on the search path as `<name>.toml` (descriptions that ship with Styx are listed in
`styx_sensor::BUILTIN_DESCRIPTIONS`, today the OV9782's, and come after the search path, so an
installed file overrides them), and bind the generic bridge to the sensor's I²C
address in the device tree (`kernel-modules/styx-sensor-bridge`, overlay template there). The
bridge makes the receiver see a sensor subdevice; Styx powers the sensor, writes its
registers and starts and stops it when the receiver asks (`PROTOCOL.md`). The registers go
through Lemnos (`lemnos_hal::register::I2cRegisters` on `lemnos_linux::hal::I2cBus`, the
driver's bus as is): the
sensor's address is claimed with `I2C_SLAVE`, never forced, so opening fails with `EBUSY`
while a kernel driver is still bound to it. On a board without the bridge, the same
description runs over any embedded-hal bus (`SensorDescription::i2c_registers`) with
`styx_hal::BoardPins` over Lemnos GPIO lines (`lemnos_linux::hal::GpioLine`) or the chip HAL's
pins.

Check every format's timing and embedded line on the board, not only the one you started
with. A format's `pixel_rate` is the rate the sensor's system clock gives, which is not always
what a kernel driver reports (the OV9782's raw8 runs at 192 MHz, its driver says 200 MHz: 4%
slow frames). An embedded layout written for one bit depth may not hold in another (the
OV9782's raw8 line carries each value's top 8 bits only): `embedded_data = false` under that
`[formats.<name>]` leaves the line uncaptured there, and frames report predicted values.

### The `[bus]` section (boards without a device tree)

On Linux the receiver's device tree endpoint says how the sensor is wired (lanes, clock mode),
and the description needs nothing about it. On a microcontroller nothing does, so the
description says it in an optional `[bus]` section, read by the platform's receiver
(`SensorDescription::bus`, `BusSection::to_hal` gives a `styx_hal::Bus`); Linux ignores it:

```toml
[bus]
# A parallel (DVP) port: data width 8 to 16 bits; pclk_rising and hsync_active_high default
# to true, vsync_active_high and embedded_sync (BT.656 codes) to false.
parallel = { width = 8, pclk_rising = true, hsync_active_high = true, vsync_active_high = false }
# or MIPI CSI-2: 1 to 4 data lanes, continuous_clock (default true), virtual_channel (default 0).
# The link frequency comes from the format (`formats.<name>.link_frequency`).
# csi2 = { lanes = 2, continuous_clock = true }
```

Exactly one of `parallel` and `csi2`; validation reports a missing or doubled bus, widths
outside 8-16 bits, lane counts outside 1-4 and virtual channels above 15.

For a target without `std`, compile the description at build time instead of parsing TOML on
the device: `styx_sensor::build::compile(&["sensors/ov5640.toml"])` in `build.rs` (feature
`build`) validates it (a broken description fails the build) and writes its compact binary
form, which the firmware reads with
`SensorDescription::from_postcard(styx_sensor::include_description!("ov5640"))` (feature
`postcard`). See [portability.md](../portability.md).

## A camera with a focus motor

Modules with autofocus move their lens with a voice-coil motor (VCM): a small I²C chip
(DW9714, DW9807/DW9817, AK7375, …) that takes a position code (10 or 12 bits) and drives the
coil. VCMs report no position; Styx predicts it from the moves it wrote and the lens's settle
time. AF (`styx-algo`, see [algorithms.md](algorithms.md), "AF: autofocus") then drives it in
dioptres through the processed captures' controls (`AF_MODE`, `AF_TRIGGER`, `AF_STATE`,
`LENS_POSITION`, `AF_WINDOWS`, `AF_METERING`, `AF_RANGE`, `AF_SPEED`;
`examples/01_capture/camera_controls.rs`).

**The lens has a kernel driver** (`dw9807-vcm`, `ak7375`, `dw9714`; the device tree overlay
of the module binds it, e.g. `dtoverlay=imx708` for the Camera Module 3, whose DW9817 sits at
`0x0c` under `dw9807-vcm`). The driver registers a `MEDIA_ENT_F_LENS` subdevice that the
sensor links to with an ancillary link; Styx finds it at discovery (`styx_native::lens::
find_kernel_lens`, listed as the `lens` property), reads the `V4L2_CID_FOCUS_ABSOLUTE` range
and moves the lens with that control. What the driver does not report goes in the sensor's
data file:

```toml
# imx708.kernel.toml
pdaf = "imx708"            # phase detection data in the embedded data (IMX708 layout)

[lens]
drivers = ["dw9807"]       # lens entity names (first word) this applies to; empty: any
range = [0, 1023]          # default: the control's range
settle_us = 12000          # a move settles within this
delay = 2                  # frames from writing a move to the first frame exposed there
map = [0.0, 445, 15.0, 925]   # dioptres -> position, if the AF tuning has no map
```

**Styx drives the VCM** (a sensor behind the bridge, or a lens without a kernel driver): the
sensor description gets a `[lens]` section with the chip on I²C, and Styx drives it with
Lemnos's VCM driver (`lemnos-drivers-vcm`) on the sensor's bus; Styx schedules the moves
(`styx_sensor::lens`):

```toml
[lens]
settle_us = 10000
map = [0.0, 120, 10.0, 900]
i2c = { address = 0x0c, chip = "dw9714" }   # dw9714, dw9807, dw9817, ak7375, or custom:
# i2c = { address = 0x0c, chip = "custom", format = { register = 0x03, bytes = 2, shift = 0,
#         bits = 10, power_up = [[0x02, 0x00]], power_up_us = 1000, power_down = [[0x02, 0x01]] } }
```

Built-in formats (Lemnos's `VcmChip`): DW9714 (no register, `code << 4`, bit 15 powers down),
DW9807 and DW9817 (registers 0x03-0x04, control 0x02: 0 on, 1 off), AK7375 (registers
0x00-0x01, `code << 4`, 12 bits; control 0x02: 0 active, 0x40 standby). A custom format has at
most four power-up and four power-down writes. The DW9807's busy flag (register 0x05) is not
polled: moves come at most once a frame.

**Frame-exact moves.** A move asked for frame `F` (`ControlHandle::request_lens_at`) is written
at the start of frame `F − delay` (at once when that has passed, or before streaming); each
frame's `FrameControls::lens` says where the lens was during its exposure (from the frame-start
time, the exposure, the readout time and the moves) and whether it had settled. AF measures a
scan step on the first settled frame. Choose `delay` so a move written at a frame start has
settled before the exposure of the frame `delay` later begins (2 at 30 fps with a 10-15 ms
VCM; 3 at 120 fps).

**The dioptre map** says which code focuses where: `0 D` (infinity) and a near distance, as
measured on the module (focus on a far target and on a chart at a known distance with
`LENS_POSITION` in manual mode, read the codes). Raspberry Pi's tunings carry it as
`rpi.af.map` (imported as the tuning's `af.map`, which wins over the lens's). Without any, AF
assumes the lens's whole range spans the normal focus range (0-12 D), which scans more than it
needs to.

**Tuning.** A Raspberry Pi tuning's `rpi.af` converts (`imx708.json`'s: ranges, speeds, PDAF
gain and confidence thresholds, the map); without an `af` section AF uses the defaults (the
IMX708 module's, which are reasonable for a phone-style VCM). A sensor without phase detection
uses contrast scans; set `dropout_frames = 0` in its tuning's speeds to skip the PDAF attempt.

**Checking it** (none of this has run on hardware yet; the Camera Module 3 is the first
target, TODO.md): with the camera in manual mode, step `LENS_POSITION` and check the image
sharpens at the expected distances (the map), that a move lands on the frame
`FrameControls::lens` reports as settled (the settle time and `delay`), then one-shot and
continuous AF on a near and a far target; with the IMX708, that `ControlHandle::pdaf` returns
cells with confidence and that the PDAF loop moves the lens the right way (the sign of
`pdaf_gain`).

## Tuning a camera

A new sensor runs with Styx's generic tuning (grey-world white balance, identity colour
matrix, assumed noise) until it has its own. `styx-tune` makes one from raw captures of a
ColorChecker, flat fields under lights of known colour temperature and dark frames:
`styx-tune capture` records them through Styx, `styx-tune calibrate` writes `<sensor>.toml`
(and a Raspberry Pi `.json`); install it as `/etc/styx/tuning/<name>.toml`, where `<name>` is
the stem of the description's `tuning =` entry. Equipment, shots and the comparison with
libcamera's `ctt`: [docs/tuning.md](../tuning.md).

## Checking a camera

* `bridge_capture` (`crates/native/examples`): lists every camera with its modes and exact
  frame rate ranges, streams 30, 60 and 120 fps and prints the measured rate.
* `landing [fps] [rounds] [key]`: steps exposure, gain, both at once (exposure ×2, gain ÷2)
  and the frame rate, half the requests written at once, half at frame starts, and checks
  from the raw image levels that every frame's level / (exposure × gain) stays constant
  and every frame lasted the duration it reports: a wrong delay shows as frames off by the
  step (±50-100%). No embedded data needed.
* `native-pipeline pisp --cold` (`tools/native-pipeline`): the PiSP path with 3A, start-up,
  convergence and CPU.

## The OV9782 on the CM5: kernel driver against the bridge

The same OV9782 on cam0, once under the upstream `ov9282` driver (Raspberry Pi kernel
6.12.47; `systemctl stop styx-bridge`, `helios-peripherals` stopped; entity
`ov9782 10-0060`, no metadata pad, so no embedded data) and once behind the bridge (with
embedded data), back to back in the same scene (a dim room, warm lamp, 100 Hz light) on
2026-10-02 (device clock Aug 17), the HeliOS tuning (`ov9782.json`), PiSP path (NV12
1280x800 + RGB 640x400, temporal denoise on), `native-pipeline pisp` unless noted. The data
file is the built-in `ov9782.toml` (delays 2/2/1, the bridge's measured values).

What the driver reported: codes `SBGGR10_1X10`/`SBGGR8_1X8` at 1280x800, 1280x720, 640x400;
`PIXEL_RATE` 160 MHz, `LINK_FREQ` 400 MHz, `HBLANK` 176.., `VBLANK` 41.. (1280x720 set) or
110.. (1280x800), `EXPOSURE` 1..`frame length - 25`, `ANALOGUE_GAIN` 16..255, `HFLIP`/`VFLIP`
on by default and without `MODIFY_LAYOUT` (their codes are right with the flips off, which
Styx sets at open).

| | kernel driver | bridge |
|---|---|---|
| 30 / 60 / 120 fps asked: measured (`bridge_capture`, 120 frames) | 30.000 / 59.983 / 119.967 fps (predicted 30.0000 / 59.9837 / 119.9674), no drops | the same |
| `landing`, 30 fps: steps on the predicted frame; frames whose level / (exposure × gain) is within 5%; frame periods within 1% of the reported duration | 24 of 24; 144 of 144; 143 of 143 | 16 of 16; 96 of 96; 95 of 95 |
| `landing`, 60 / 120 fps | 24 of 24 and 24 of 24; level within 5% on 140 / 125 of 144 (the others 5-9% off: the black level settling after gain changes at 60, 100 Hz light on 3.3 ms exposures at 120; a missed frame is 50-100% off); periods 143 of 143 | |
| control delays (exposure, gain, frame length) | 2, 2, 1 (as the bridge) | 2, 2, 1 |
| values a frame reports | predicted (delay model) | read back (embedded data) |
| open → first frame, cold, 30 fps | 37.5-39.8 ms | 29.5-30.1 ms |
| where | configure 0.3 ms (no I²C from Styx), `STREAMON` 20.6-21.2 ms (the driver powers the sensor and writes its mode tables in `s_stream`, at 100 kHz), `STREAMON` → first frame 14.1 ms | configure 12.1 ms (bring-up over I²C), `STREAMON` 6.5 ms, → first frame 9.3 ms |
| AE from cold, 30 fps: exposure / output within 5%, locked; open → locked | 5 / 4 / 6 frames, 237.6-243.6 ms (5 runs) | 5 / 4 / 6, 229.6-230.1 ms (3 runs) |
| darker step (¼) / brighter step (3×), 30 fps | 2 / 1 / 3 and 0 / 0 / 0 | the same |
| AE from cold, 60 fps: locked | frame 6 (142 ms) | frame 6 (130 ms) |
| AE from cold, 120 fps: locked | frame 8, 11, 14, 15, 16, 18 (6 runs, 110-188 ms) | frame 8 (3 of 3, 96 ms) |
| warm restart in place / reopen at 120 fps: first frame, locked | 44.4 ms, frame 7 / 44.4-46.4 ms, frame 2-6 (61-96 ms) | (pipeline.md: 24 ms, frame 2 / 51 ms, frame 2) |
| CPU per frame, tool, not reading the output: 30 / 120 fps | 0.260 / 0.220 ms | 0.270 / 0.242 ms |
| Styx API (`native_isp_bench single`), NV12 1280x800: process CPU, latency median, 30 / 120 fps | 0.23 ms, 9.52 ms / 0.20 ms, 9.53 ms | 0.29 ms, 9.51 ms / 0.18 ms, 9.51 ms |
| frame start → outputs (tool), 30 fps | 9.91 ms median | 9.91-9.92 ms |
| software ISP path, 30 fps: CPU, latency, locked | 3.1 ms, 10.49 ms, frame 8 | 3.1 ms, 10.49 ms, frame 8 |
| AE's final total exposure in this scene | 12-15% higher (e.g. 33.1 ms × 2.25-2.38) | 33.2 ms × 2.00-2.06 |

Notes:

* At 120 fps the kernel path's AE locks later in most runs. Its predictions are right: the
  measured luma divided by the predicted exposure × gain stays within ±3% from frame 4 on
  (`pisp-frames.csv` of a run), as it does on the bridge; what is left is the ±3-4% the
  100 Hz light puts on 8 ms exposures, which AE (5% tolerance, two frames in a row) sometimes
  chases for a few frames. The bridge's runs in the same minutes locked at frame 8 each time.
  Flicker avoidance (`--flicker auto`, the Styx default) since: under a ±20% 50 Hz lamp at
  EV -4, without it AE never locked in 150 frames, with it at frames 19-28 and moved
  exposure × gain by 2% in steady state instead of 11% (pipeline.md, "Flicker").
* The driver's register set gives the sensor about 12-15% less signal than the bridge's
  (HeliOS) registers at the same exposure and gain code; AE makes up for it with gain.
* Starting is slower by 8 ms: the driver writes the whole mode at `STREAMON` on the 100 kHz
  bus, where the bridge path writes it while the ISP opens.
* Found on the way: the PiSP path set its start values after the control schedule had
  started (the schedule started with the sensor side, before the front end's `STREAMON`),
  so frames 0 and 1 ran with the driver's defaults and warm restarts began at 5.8 ms × 1.
  The schedule of a kernel driver's sensor now starts in `resume_external`, after the
  `STREAMON` that starts the sensor, and values asked for before are set at once.
* pipeline.md's bridge figures were taken on other days (open → first frame 52.6 ms then);
  the bridge column here is from the same session as the kernel column.

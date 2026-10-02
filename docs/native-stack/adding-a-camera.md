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
| IMX708 | `imx708` | libcamera's helper, CCS embedded data | no |
| OV5647 | `ov5647` | libcamera's helper | no |

Without a data file the camera still works: gain is taken as linear with the driver's
minimum code as 1×, delays as libcamera's defaults for unknown sensors (exposure 2, gain 1,
frame length 2), no black level (the tuning's, if any). A new file is needed only to get
these right; check them with `landing` (below).

What happens when it runs (`crates/native/src/kernel.rs`):

* **open**: the subdevice is opened (an advisory lock makes it exclusive between Styx
  processes) and the flips are set to the description's defaults (off) before anything sets
  a format.
* **configure**: the sensor format is set, the ranges re-read, the mode's defaults set as
  controls (`VBLANK`, `EXPOSURE`, `ANALOGUE_GAIN`), and frame 0's values queued.
* **start**: frame 0's values are set (the driver applies them as it starts), then the
  receiver's `STREAMON` starts the sensor (`s_stream`). There is no bridge to serve.
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
Put it on the search path as `<name>.toml`, and bind the generic bridge to the sensor's I²C
address in the device tree (`kernel-modules/styx-sensor-bridge`, overlay template there). The
bridge makes the receiver see a sensor subdevice; Styx powers the sensor, writes its
registers and starts and stops it when the receiver asks (`PROTOCOL.md`).

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

(see below)

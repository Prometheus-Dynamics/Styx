# Processing: ISPs, settings, outputs, stills and 3A

A sensor gives raw Bayer frames. An ISP makes pictures of them: RGB24, NV12, I420 or luma. Every
ISP reads the same neutral settings (`styx_settings::IspSettings`), and each ISP family maps them
to its own hardware or arithmetic. Styx holds the values; Lemnos holds the hardware layouts.

| ISP | Crate | Runs on | State |
|---|---|---|---|
| Soft ISP | `styx-soft` | any CPU (scalar, SSE2/SSSE3/AVX2, NEON); `no_std` + `alloc` | run on a board, in the service and in `styx-measure` ([board-runs.md](board-runs.md)) |
| GPU ISP | `styx-gpu` (feature `vulkan`) | any Vulkan device | host: lavapipe, and a desktop GPU by hand |
| PiSP | `styx-pisp` | the Raspberry Pi PiSP front and back end, through Lemnos | run on a board ([board-runs.md](board-runs.md)) |
| Rockchip ISP21 / ISP30 | `styx-rkisp` | Rockchip boards, through Lemnos | untested on board |
| K230 ISP8000 | `styx-k230` | Canaan K230D, through Lemnos | untested on board |
| ISP600 | `styx-isp600` | the black level and enables only (see below) | untested on board |

## The settings: `styx-settings`

`IspSettings` is the one type every ISP maps from, in hardware-independent terms with full scale
1.0. It holds:

- black level, white balance, digital gain and the colour matrix;
- the tone curve and lens shading;
- denoise and sharpening;
- the luma histogram weights;
- the outputs' sizes and formats.

The 3A controller writes it, and a settings JSON from the controller reads here unchanged.
`changed_fields` names what differs between two values. `interpolate` blends two tunings.

## Soft ISP: `styx-soft`

The whole pipeline in Rust, each stage optional:

1. Unpack: CSI-2 RAW10 or RAW12, 16-bit, or 8-bit samples.
2. Black level, white balance, digital gain and lens shading, folded into one multiply per
   sample.
3. Demosaic: bilinear, Malvar-He-Cutler or edge-directed. `Scale::Half` turns each 2x2 quad into
   one pixel instead.
4. Colour matrix.
5. Tone curve.
6. Optional denoise and sharpening.
7. Output.

The 3A statistics (zones, histogram) come out of the same pass. They are handed to the
controller as before the white balance and digital gains, as the back end's are: the zone sums
divided by each channel's gain, and the histogram's bins moved down by the gain a grey quad took.

The default arithmetic is integer with an exact tone table. It gives the same bits on every host
and on the GPU ISP. `Half` (fp16) and `ApproxTone` are chosen only by name, and no CPU feature
changes the result.

`SoftIspStage` is the frame interface: a raw lease in, a picture lease the caller acquired out,
and no copy of the input. `with_threads` processes row bands on a persistent pool. A warm frame
allocates nothing.

## GPU ISP: `styx-gpu`

The soft ISP's pipeline as Vulkan compute shaders, bit for bit the same integer arithmetic:

- Same interface: `GpuIspStage` is the GPU twin of `SoftIspStage`.
- No copy in: a raw frame on a dma-buf is imported once per buffer and read in place.
- One submission and one fence wait per frame.
- GPU sharing: an optional lease hook (`GpuLease`) lets a product share the GPU.
- Builds anywhere: the SPIR-V is committed, so a build needs no shader compiler. Everything sits
  behind the `vulkan` feature, and with no Vulkan device the tests skip and say why.

## PiSP: `styx-pisp`

The neutral settings mapped into the Lemnos library's PiSP front and back end configuration:

- black level, white balance and gain, the colour matrix, tone and lens shading;
- the denoise blocks (temporal denoise follows exposure) and sharpening;
- the YCbCr conversion of each output, and two outputs (full size, and output 1 as a 2x2 box).

The layouts and tiling are the library's. A block the library does not carry is refused
(`PispError::Unsupported`), never written here. A setting the back end has a block for, but whose
values no source Styx may use gives a meaning, is left out of the frame and named
(`LeftOut`). The frame is made with everything else. Today that is `denoise.dpc`: the uapi names
the defective pixel coefficients but says nothing of what they do, so the block stays off.

The configuration bytes equal `dev`'s for the recorded settings, every block (`tests/dev_golden.rs`).
The live path is tested against a simulated ISP.

## Rockchip and K230

- **`styx-rkisp`:** maps the settings to parameter buffers in Lemnos's ISP21 and ISP30 layouts,
  posts the job on `camera.isp/1`, and decodes the statistics. ISP32, ISP35 and ISP39 open, then
  refuse at their first apply (`Error::Unmapped`): the library does not state their colour and
  statistics units.
- **`styx-k230`:** maps the settings to register writes in Lemnos's `k230isp` layout, checked by
  Lemnos's own `config::check` before they can reach the block. Lens shading is not written yet,
  because the library has no layout for its table. A `lens_shading` setting is ignored on this ISP
  until it does.

## ISP600

No open source exists for most of this block's register encodings, so `styx-isp600` writes only
the black level's enables and bit depths, the module enables, and the temporal denoise's record
enable. Every other setting is refused with `hardware ISP tuning unavailable: no open source for
the encodings`, naming the setting. No statistics are decoded.

On this family, raw frames go to the soft or GPU ISP, with Styx's own 3A.

## ISP in the camera service

A camera table can ask for a processed stream beside the raw one:

```toml
[camera."lemnos:sensor1"]
isp = "auto"                 # auto | pisp | soft | none
isp_format = "NV12"          # output 0 (default NV12)
isp_size = "1280x800"        # output 0's size (default: the raw frame's)
isp_second = "NV12:640x400"  # output 1, published too (default: not published)
```

- **Which ISP:** `auto` takes the PiSP back end when the camera's chain has one that takes the
  geometry, and the soft ISP otherwise. After `isp_refusals` back-end refusals in a row it
  switches to the soft ISP, and it tries the back end again on a backoff (`isp_retry_start_ms`
  doubling to `isp_retry_cap_ms`). The status names the ISP in use and every refusal's reason
  (`isp=`, `isp_reason=`). Each frame's metadata says what made it: the camera, the PiSP or the
  soft ISP.
- **Fallback, said out loud:** while `auto` runs the soft ISP because the back end refused,
  `isp_fallback=` carries the refusal. It is `-` when the back end makes the frames.
- **Left out:** `isp_left_out=` names each setting the ISP in force does not apply, and why.
- **Frames made against frames asked:** `isp_made=<made>/<asked>`. While frames go without a
  picture, `isp_short=` says why, with the ISP's measured time per frame against the camera's
  frame interval, for example a soft ISP at 16.6 ms per frame on a 120 fps camera.
- **Sockets:** each output is published on its own socket (`isp0_socket=`, `isp1_socket=`).
- **The input copy:** the back end's input is one copy of the raw frame, counted at the copy site
  `pisp-be-input` with the bytes read and written.
- **Pool size:** the ISP's hand-off takes buffers from the camera's pool. A pool too small for it
  is refused by name, and the raw stream still runs. With 9 buffers the raw reserve is 7, and raw
  readers together may hold one frame older than the newest. With 8 buffers `isp` is refused. The
  arithmetic is in `crates/out/service/src/isp.rs` (`hold_for`).

The service runs no 3A yet (`isp_3a=none`). The processed stream uses neutral settings
(`IspSettings::neutral(0.0)`): black level 0, unity gains, the identity colour matrix and the sRGB
tone curve, until the controller is wired.

## Outputs for several consumers: `styx-outputs`

Consumers ask for a format at a size (`Ask`), and `plan` decides which ISP output serves each one:

- **Exact match:** an output that makes that format at that size serves it.
- **Shared output:** consumers asking for the same thing share one output, each holding a share of
  the same lease.
- **Converted:** the same size in another format of the same family goes through `styx-scale`,
  reported as a conversion.
- **Refused:** anything else is refused with the reason.

`Distributor` then runs one ISP pass per configured output per frame. Captures are never upscaled.
Crops and pyramids are graph work, not this crate's.

## Stills: `styx-still`

`serve` makes a still from the next frame the camera's loop takes:

- **Raw DNG:** a DNG 1.4 file with black and white levels, the as-shot neutral, colour calibration,
  exposure, ISO and lens shading opcodes.
- **Processed:** the frame at full size through the soft ISP, with Malvar-He-Cutler and BT.601
  full range.

It never waits and never owns a camera. Stills are one-offs, so they allocate. The preview
stream's frames are not copied for them.

## 3A

- **The algorithms:** `styx-aaa` holds them (AE, AWB, lens shading, colour, AF). They are driven
  by tuning data, deterministic, and replayable from recorded statistics.
- **The loop:** `styx-controller` turns each frame's statistics and metadata into the sensor
  request for a named frame, the lens move, and the frame's `IspSettings`.
- **Lenses:** `styx-focus` moves a Lemnos `lens/1` lens for AF.
- **Tuning files:** `styx-tuning` finds a camera's tuning file. It looks in `STYX_TUNING_PATH`,
  then `/etc/styx/tuning`, then the bundled `/usr/share/styx/tuning`, then the generic tuning,
  then the defaults. It records every file it skipped and why. A Raspberry Pi JSON tuning is
  skipped with that reason; `styx-tune` writes Styx's TOML form.

- **In the service:** `aaa = true` beside `isp` runs the loop on a native camera's processed
  stream (off by default: exposure and gain stay as set, and the ISP runs neutral settings).
  - Statistics: the PiSP route counts them in its input pass (the copy it already makes, every
    other quad of the rows counted, 32x32 zones); the soft ISP gathers its own on the first
    picture it makes. Neither reads the frame again.
  - Their cost: they are taken on one frame in `aaa_stats_every` (default 4: 30 a second at
    120 fps, the rate the tunings' speeds are made for) and counted on one quad row in
    `aaa_stats_rows` (default 4: about 31 quads in each zone at 1280x800 on the PiSP route, above
    the 16 a tuning's zone needs; fewer rows on a frame too short for every zone row to have one).
    A frame without them costs nothing extra. The loop steps on the frames that have them; the
    status says the sampling as it runs (`aaa_stats=<every>/<rows>`). At the defaults the loop
    settles from a dark start, and after the light steps four times brighter, within 0.1-0.17 s
    on the simulated frames (`known_frames.rs`).
  - The step runs on the ISP thread after the frame is published. Its exposure and gain go
    to the sensor through the native controls for a named frame; its `IspSettings` apply to the
    next frame (never part of one). The frame rate stays as set.
  - Controls: `ae`, `ev`, `ae_exposure_max_us`, `ae_gain_max`, `awb`, `red_gain`, `blue_gain`,
    `colour_temperature` and `flicker`, set at run time on the control socket. A manual `exposure`
    or `gain` wins from the frame it is set for, even when the loop skips frames (the capture
    thread posts it for that frame itself); `auto` gives it back.
  - White balance outside the tuning: when fewer zones than the tuning's `min_regions` are within
    `delta_limit` of the grey point the search chose, and of the point the zones alone choose (the
    search without the prior), no point fitted the data and the lux prior alone chose. AWB then
    takes the tuning curve's point nearest the zones' mean, and the status gives that mean (`awb_outside=<r/g>/<b/g>`, thousandths). On Raze #2's bench lamp the scene read R/G 1.49,
    B/G 0.22 against the curve's warm end of 0.95 / 0.42. The prior had picked 6155 K, which turned
    the picture further orange. The nearest point is 2860 K, which goes the right way as far as
    the tuning reaches.
  - Tuning: `tuning = "<sensor>.toml"`, searched in `tuning_dirs` first. The status says which
    file ran (`aaa_tuning`) and why the loop stopped, if it did (`aaa_reason`).
  - A raw reader beside a processed one gets the same exposure and gain: they are the sensor's.

# PiSP natively (phase 3 groundwork)

The Raspberry Pi 5 / CM5 ISP ("PiSP") driven from Rust with no libcamera and no libpisp:
`crates/pisp` (`styx-pisp`) and the on-device tool `tools/pisp-spike`.

## Hardware and nodes

PiSP is two blocks with separate drivers.

**Front end (FE)**, part of `rp1-cfe` (RP1 south bridge, one per CSI-2 port). It sits between
the CSI-2 receiver and memory, works on the live stream (line by line, no tiling) and writes
raw frames plus statistics. `/dev/media0` on the CM5 (driver and model `rp1-cfe`, hw 0x114666):

```
"ov9782 10-0060" (sensor) :0 ──► "csi2" :0            (immutable)
"csi2" :4 ──► "rp1-cfe-csi2_ch0"  (/dev/video0)  raw to memory, no ISP
"csi2" :4 ──► "pisp-fe" :0                        (FE input; also :6/:7 for channels 2/3)
"csi2" :5 ──► "rp1-cfe-embedded"  (/dev/video1)
"rp1-cfe-fe_config" (/dev/video7) ──► "pisp-fe" :1   META_OUTPUT  RPFC  struct pisp_fe_config (632 B)
"pisp-fe" :2 ──► "rp1-cfe-fe_image0" (/dev/video4)  VIDEO_CAPTURE  raw output 0
"pisp-fe" :3 ──► "rp1-cfe-fe_image1" (/dev/video5)  VIDEO_CAPTURE  raw output 1
"pisp-fe" :4 ──► "rp1-cfe-fe_stats"  (/dev/video6)  META_CAPTURE RPFS  struct pisp_statistics (23200 B)
subdevs: csi2 /dev/v4l-subdev0, pisp-fe /dev/v4l-subdev1, sensor /dev/v4l-subdev2
```

**Back end (BE)**, `pispbe` (in the BCM2712), memory to memory, processes an image in tiles
of at most 640 pixels wide. Two identical node groups (`/dev/media2`, `/dev/media3`, driver
and model `pispbe`, hw 0x2252701 = BCM2712 D0) share the hardware; each has an entity
`pispbe` with immutable links to:

```
pispbe-input         VIDEO_OUTPUT_MPLANE   Bayer (16-bit, PISP_COMP1) or RGB/YUV input
pispbe-tdn_input     VIDEO_OUTPUT_MPLANE   temporal denoise long-term average (read)
pispbe-stitch_input  VIDEO_OUTPUT_MPLANE   HDR stitch previous exposure (read)
pispbe-output0       VIDEO_CAPTURE_MPLANE  main output (YUV/RGB/Bayer)
pispbe-output1       VIDEO_CAPTURE_MPLANE  second output (has the downscaler)
pispbe-tdn_output    VIDEO_CAPTURE_MPLANE  TDN average (write)
pispbe-stitch_output VIDEO_CAPTURE_MPLANE  stitch (write)
pispbe-config        META_OUTPUT  RPBC     struct pisp_be_tiles_config (16720 B)
```

## Buffer formats

- **CSI-2 into the FE is always 16 bits per sample.** The receiver unpacks the sensor's
  RAW10 (the `csi2` source pad is set to the 16-bit code, e.g. `SBGGR16_1X16`; the `pisp-fe`
  sink pad too). Values are MSB-aligned (10-bit value << 6), so black level 64 is 4096.
- **FE raw outputs**: `V4L2_PIX_FMT_SBGGR16` (`BYR2`, 2 bytes/pixel, little-endian) or the
  8-bit PiSP compressed `PC1B` (`PISP_COMP1_*`, delta compression, `compress.mode = 1`,
  offset 2048 in libcamera). PiSP's own "BPS_10" packing (3 samples in 4 bytes) is **not**
  MIPI packing, so `pBAA` from `csi2_ch0` cannot go into the BE.
- **FE statistics** (`struct pisp_statistics`): AWB 32x32 zones of `{R, G, B, counted}` sums
  plus 4 floating regions; AGC 512 row sums, a 1024-bin weighted Y histogram and 4 floating
  `{Y_sum, counted}`; CDAF 8x8 figures of merit plus 4 floating. Sums are over Bayer quads
  (one R, one G, one B value per quad; R and B match the raw frame exactly, the G value is
  within 0.8% of the two greens' mean, combination undocumented) after the statistics black
  level (BLC); `counted` counts quads.
- **BE outputs**: YUV420/NV12/NV21/YUYV/UYVY/YUV422P/YUV444 (single or multi-planar), RGB24/
  BGR24/RGBX/XBGR/RGB48, Bayer 8/16, Y8/Y16. For non-`M` formats (e.g. `NV12`) there is one
  V4L2 plane and the driver computes the chroma addresses; the config's `stride2` must match.

## Config flow

### Front end (per frame)

`rp1-cfe` runs a "job" only when **every node with an enabled link is streaming and has a
buffer queued**, so with the FE path enabled a config buffer must be queued for every frame
(libcamera keeps `num_cfe_config_queue` of them ahead). At `QBUF` the driver copies the config
and validates it (`pisp_fe_validate_config`: input enabled and streaming, outputs no larger
than the node formats, non-zero crops). At job time it fills in the stats and output buffer
addresses and `ilines`, then writes to the registers: always the buffers, global and input
blocks; enabled crop/output blocks; other blocks only if their bit is in `dirty_flags` /
`dirty_flags_extra`. A block whose settings never change can therefore be written once.
The sensor is started when the last node starts streaming (`s_stream`).

Setup order: links (disable everything else, enable `csi2:4 → pisp-fe:0`, config, stats,
image0), sensor format, `csi2` pad 0 = sensor format, `csi2` pad 4 = 16-bit code, `pisp-fe`
pad 0 (propagates to 2/3), node formats, `REQBUFS`, queue, `STREAMON` on every node.

### Back end (per job)

A job is formed when the config node and the main input have a buffer, plus each of output0,
output1, TDN and stitch whose enable bit is set in the queued config. At `QBUF` of the config
the driver copies `pisp_be_tiles_config` and checks it against the node formats (strides and
sizes of enabled outputs, number of tiles 1..64). It then writes the buffer addresses and the
**effective** enables itself (it clears TDN/stitch/output enables whose buffers are missing),
copies the rest of `pisp_be_config` from `global.bayer_order` onwards into the registers,
points the hardware at the tile array and queues the job. The first 112 bytes of
`pisp_be_config` (old address fields) are ignored.

The configuration reaches the hardware only through registers (MMIO); the tiles are the only
part it reads from memory (DMA, from the driver's coherent copy). The stock driver writes all
1589 configuration words with `writel()` on every job: 117 µs of CPU per job on the CM5
(113.8 µs of it the 71.6 ns barrier-plus-store per word), on the thread that queues the last
buffer of the job, so on the frame's latency too. Styx's patched build of the driver
([`kernel-modules/pispbe`](../../kernel-modules/pispbe/README.md), installed on the dev box
as an override of the image's module) writes the same values with relaxed MMIO writes and
skips words equal to the ones last written (the registers keep their values between jobs; it
forgets them when the clock is gated), and reads the configuration from a cached copy:
7-8 µs per job, identical outputs. With it a config needs no dirty tracking on our side:
writing an unchanged block costs nothing.

The tiles are the userspace's job: each `pisp_tile` gives the input window (with 16 pixels of
context each side for the Bayer/RGB pipeline), per-output crops that remove the context,
downscaler/resampler input sizes and initial phases, the output window and byte offsets.

Several jobs per raw frame (Styx's extra passes for regions of interest): the input node is
memory to memory, so the same raw buffer can be queued again once the previous job finished
(not while it is queued: vb2 refuses a buffer index twice). A job may enable only one output
(the other node then gets no buffer: the driver skips its buffer check, and a buffer queued
there anyway is pulled into the job and handed back); `BackEndStream` queues buffers only for
the outputs a config enables. A job with `TDN_INPUT` and without `TDN_OUTPUT` reads the
average without writing one (only the TDN input node needs a buffer). Each output's crop is per
job; the tiles cover only the input the enabled outputs need, so a job for a 128x128 region
takes 0.05 ms where the whole 1280x800 frame takes 2.26 ms (with temporal denoise and both
outputs). The driver's shadow of the last register values makes alternating configs cheap:
only the words that differ (enables, crops, formats, TDN) are written. See
[pipeline.md](pipeline.md#extra-back-end-passes-regions-of-interest).

## What libcamera does (`src/libcamera/pipeline/rpi/pisp/pisp.cpp`)

- Match: `rp1-cfe` with a sensor, and a free `pispbe` node group.
- `configureEntities`: `csi2 → pisp-fe` enabled and `csi2 → csi2-ch0` disabled; config,
  image0 and stats links enabled, image1 disabled; `csi2` routing image stream to the video
  source pad (and embedded data to the meta pad when the sensor has it); `csi2` sink = sensor
  format, `pisp-fe` sink and `csi2` source = the 16-bit code, `pisp-fe` pad 2 = the output
  code (16-bit or compressed).
- `configureCfe`: FE input `streaming = 1`, `BPS_16`; output 0 = the `fe_image0` format
  (16-bit or `PISP_COMP1` with compression enabled); `DECIMATE` statistics above the
  downscaler width limit.
- The IPA (`src/ipa/rpi/pisp/pisp.cpp`) sets the statistics: crop to the whole mode, AWB
  pixels above 98% excluded, CDAF on Gr/Gb, AGC zone weights from the tuning (15x15 mapped to
  16x16, 4-bit, two per byte), floating region 0 over the frame (for lux); RGBY weights =
  BT.601 luma times the AWB gains; FE `BLA` = per-channel black level (output black level =
  the minimum), `BLC` and BE `BLC` = the minimum. Per frame it programs BE WBG, CCM, gamma,
  LSC, CAC, DPC, SDN, TDN, CDN, GEQ, sharpening, saturation and tonemap from the algorithms.
- `configureBe`: input = the CFE format (compressed → `DECOMPRESS`); per output a CSC (the
  YCbCr matrix for YUV, an R/B swap for libcamera's `RGB888`, which is V4L2 `BGR24`), smart
  resize to the output size, output clipping (limited range for SMPTE170M/Rec709); TDN and
  stitch formats set in case they are turned on; YCbCr and inverse "jpeg" (full-range
  BT.601) for the internal YCbCr stage.
- Per frame: a CFE job's raw buffer goes to `pispbe-input` with a freshly prepared config
  (`BackEnd::Prepare`), TDN and stitch buffers ping-pong. 16-bit sensors need an endian swap
  and 14-bit ones an unpack on the CPU (hardware gaps).

## styx-pisp

- `uapi`: `#[repr(C)]` mirrors of every FE, statistics and BE structure; `bytemuck::Pod`
  (no implicit padding) and compile-time size/offset assertions generated from the device's
  kernel headers by `crates/pisp/layout-check/run.sh <linux-tree>` (a C program kept out of
  the crate build; the script diffs its output with `src/uapi/layout.rs`).
- `fe::FrontEnd`: setters with dirty tracking, `default_stats(black, wb_r, wb_b)` (libcamera's
  start-up statistics set-up), `prepare()` fills AWB/AGC/CDAF grids, LSC scale, downscaled
  output sizes, strides and decimation (libpisp `frontend.cpp` rules).
- `stats::Statistics`: decoded AWB zones, histogram, row sums, floating regions, focus; means
  and quantiles.
- `be::BackEnd`: libpisp's block defaults (gamma curve, YCbCr matrices, Lanczos/Mitchell
  filters, sharpening, demosaic, false colour), `simple_bayer(..)` for a fixed pipeline
  (BLC, WBG, demosaic, CCM, YCbCr round trip with sharpening and false colour suppression,
  gamma, CSC for YUV outputs), smart resize (downscaler on output 1 above 2x, resampler with
  a filter chosen by the factor), finalisation checks, and `be::tiling`, a port of libpisp's
  tiling library (input, context, split, crop, downscale, resample, output stages).
  DPC, GEQ, SDN, CDN and TDN setters (`set_dpc`, `set_geq`, `set_sdn`, `set_cdn`,
  `set_tdn_format` + `set_tdn`) and `set_sharpen_scaled` (libpisp's sharpening scaled as the
  Raspberry Pi IPA scales it from `rpi.sharpen`); uncompressed TDN buffers of the input's
  format are accepted, stitch and CAC are refused for now.
- `device` (feature): `FrontEndDevice` (links, formats, queues, per-frame config feeding,
  statistics and raw frames; `next_held_raw` copies the statistics buffer out as it is, or
  not at all), `BackEndDevice` (one node group, m2m jobs, timing) and `BackEndStream` (a node
  group for a stream: dma-buf input, two outputs in cached dma-heap buffers or the driver's
  (`OutputMemory`), jobs queued and waited for separately (`process_queued` / `wait_job`),
  the config buffer from a cached heap since the driver copies it with the CPU;
  `enable_tdn` sets up `pispbe-tdn_input`/`tdn_output` with two dma-heap buffers of the
  input's format that swap every job, queued whenever the job's config enables
  `TDN_OUTPUT`/`TDN_INPUT`), and
  `profile`, optional timing of every device call.
- Per-frame configs in `styx-pipeline` (`pisp_be::BeConfigBuilder`): the back end config and
  tiles are prepared once and patched where the algorithms' settings changed; see
  [pipeline.md](pipeline.md#pisp-path) for the frame path and its costs. The stock driver writes
  the whole `pisp_be_config` to the hardware on every job (0.12 ms of CPU on the CM5); the
  patched one in `kernel-modules/pispbe` only the changed words (8 µs), see "Back end (per job)".

### Licences

- Kernel uAPI headers: `GPL-2.0-only WITH Linux-syscall-note`. Only layouts, names and
  constants are mirrored (the syscall note covers userspace use of the interface).
- libpisp 1.3.0 (`raspberrypi/libpisp`, in the Buildroot tree as `libpisp-pios_1.3.0-1`):
  `LICENSE` is BSD-2-Clause, every `.cpp/.hpp` carries `SPDX-License-Identifier:
  BSD-2-Clause` (Copyright (C) 2021 - 2023 Raspberry Pi Ltd); its copies of the uAPI headers
  are GPL-2.0 with the syscall note and the meson files CC0-1.0. BSD-2-Clause is compatible
  with MIT/Apache: ported files name their source and the licence text is in the crate docs
  (`crates/pisp/src/lib.rs`), which satisfies the notice condition for source and for
  binaries that ship the docs; a binary distribution should also carry it in its notices.
- The libcamera pipeline handler and IPA (read for behaviour only, nothing ported) are
  LGPL-2.1+ / BSD-2-Clause respectively.

Two libpisp slips found while porting (not carried over): `FrontEnd::fixOutputSize` writes the
crop height into the output width; the `BackEnd` constructor writes the inverse YCbCr matrix
into `ycbcr` (libcamera overwrites both later, so it does not show there). The kernel's FE
config map writes `sizeof(struct pisp_agc_statistics)` bytes for the AGC block (it is clamped
to the end of the register window, so harmless).

## Results on the CM5 (OV9782, kernel driver path)

`pisp-spike` (kernel `ov9282` driver in its OV9782 variant, `SBGGR10_1X10` 1280x800,
`helios-peripherals` stopped, under the device lock; `tools/pisp-spike`):

- **Front end**: configured entirely from Rust; first frame 91 ms after `STREAMON`; frames
  every 33.34 ms (30 fps; 66.89 ms at the driver's default timing on the first boot), raw
  (`fe_image0`) and statistics sequences always paired, no errors over 30-frame runs.
- **Statistics decoded and checked against the raw frame**: 32x32 AWB zones count 256000
  quads (the whole 1280x800 frame); on a lit scene the AWB means equal the raw frame's
  black-subtracted R and B means exactly (R 15302.8, B 9611.3) and G within 0.8% (13017.8
  vs 12914.9 averaging both greens; the hardware's G combination is not documented).
  Histogram 256000 pixels (p5/p50/p95 bins 92/287/401), 400 non-zero row sums
  (800 rows / `row_size_y` 2), floating region 0 and focus figures of merit follow the
  scene. With the sensor's colour bar test pattern (0x5e00 = 0x80) the zone sums show the
  bars (e.g. centre zone R 61376, G 0, B 61376) and the AWB means drop below the raw mean
  because saturated pixels (above `r_hi/g_hi/b_hi` = 98%) are left out, as configured.
- **Back end**: a captured raw frame (from memory) through `pispbe` group 0 to NV12 and to
  RGB24 at 1280x800 with the fixed pipeline (BLC 4096, grey-world WB gains from the FE
  statistics, identity CCM, default gamma, sharpening, false colour). 3 tiles
  (576 + 576 + 128 output columns, 16 context pixels). **0.75 ms per frame** (median of 50,
  max 1.04 ms, from queueing the config to dequeueing output 0); `BackEnd::prepare` including
  tiling takes 17-25 us on the Cortex-A76. The RGB output of the lit scene is a correct,
  white-balanced colour image (channel means 131.9/131.4/131.8 with grey-world gains).
- **Synthetic check** (`pisp-spike --synthetic`): a generated BGGR frame with red, green,
  blue and grey bands comes out as `[208,0,0]`, `[0,208,0]`, `[0,0,208]`,
  `[208,208,208]`, with no step across the tile boundaries at columns 576 and 1152.

## Open problems / next steps

- Algorithms (phase 4): AE/AWB from these statistics, CCM/LSC/gamma from a tuning file.
- Per-frame config: done in `styx-pipeline` (see [pipeline.md](pipeline.md)): the FE config
  queue is fed from a `FrontEnd` the 3A loop updates (black levels, RGB→Y weights), two
  configs ahead; the BE gets a fresh config per job.
- Compressed raw (`PISP_COMP1`) halves the memory traffic between FE and BE; not wired yet.
- Stitch/CAC in the BE builder (LSC: `BackEnd::set_lsc`, `be::lsc`; TDN, SDN, CDN, GEQ, DPC
  and sharpening from the tuning: see [pipeline.md](pipeline.md#quality-vs-libcamera));
  compressed TDN buffers (libcamera compresses them when the raw input is compressed); `output1` runs on the device (640x400 RGB through
  the resampler), the downscaler (below half size) only offline.
- Zero copy between FE and BE: done (`FrontEndDevice::next_held` + `image_dmabufs`,
  `BackEndStream` imports them on `pispbe-input`).
- The bridge path: runs (`styx-pipeline`'s `PispPipeline`, `native-pipeline pisp`), with
  embedded data and frame starts from `fe_image0`.

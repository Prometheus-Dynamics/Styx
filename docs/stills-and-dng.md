# Stills and DNG raw files

Styx takes stills from a running capture without stopping it, and writes raw frames as DNG
files that raw converters (LibRaw, darktable, RawTherapee, Lightroom) and calibration tools
open.

* `CaptureHandle::capture_still(&StillRequest)` / `Frames::capture_still` (blocking), or
  `request_still` (returns a `PendingStill` at once, so the caller keeps taking frames).
* `styx-dng` (`crates/dng`): a DNG 1.4 writer and reader in pure Rust, no Styx dependencies, for
  any tool (the tuning tool reads its DNGs with it).
* Examples: [`still_capture`](../examples/01_capture/still_capture.rs) (preview running, a
  JPEG + DNG, a 3-shot bracket) and [`mcap_to_dng`](../examples/01_capture/mcap_to_dng.rs)
  (a recorded raw frame to a DNG).

## The API

```rust
use styx::prelude::*;

let mut frames = Frames::nv12().fps(30).open(&camera)?;
// A JPEG and a DNG of the next frame once AE has locked; the preview keeps its rate.
let still = frames.capture_still(&StillRequest::jpeg(92).with_dng(true).settle(true))?;
still.shots[0].save_image("still.jpg")?;
still.shots[0].save_dng("still.dng")?;
// -1, 0, +1 EV, each on the frame its exposure lands on.
let bracket = frames.capture_still(&StillRequest::jpeg(92).with_dng(true).bracket([-1.0, 0.0, 1.0]))?;
// Or without blocking the frame loop:
let mut pending = frames.request_still(&StillRequest::format(StillFormat::Raw).with_dng(true));
loop {
    let frame = frames.next_frame(Duration::from_millis(100));
    if let Some(still) = pending.try_take() { break; }
}
```

`StillRequest`:

| Field | |
|---|---|
| `format` | `Jpeg { quality }`, `Nv12`, `Rgb24`, `Raw` (16-bit samples at the sensor's depth, `BG10` etc.) |
| `dng` | also write a DNG of the raw frame (native cameras) |
| `exposure` | `Current` (AE's), `Fixed { exposure, gain }`, `Bracket(ev offsets)` |
| `settle` | with `Current`: wait for AE to lock |
| `denoise` | spatial/colour denoise relative to the stream's (default 2×) |
| `dng_preview`, `dng_lens_shading` | embed an sRGB preview (≤ 640 px wide) / the lens shading table |
| `timeout`, `encoder` | how long to wait; a JPEG encoder (RG24 → MJPG) instead of the registry's |

Each `StillShot` has the image, the DNG bytes and `StillMeta`: frame sequence, timestamp,
exposure, analogue and sensor digital gain, the ISP's digital gain, white balance gains,
colour temperature, lux, the bracket's EV, the frame the exposure was to land on, whether it
did (`landed`), whether the values were read back from the frame (`verified`), what
processed it (`pisp`, `software`, `stream`) and how long that took.

## How it works

### Processed native cameras (PiSP or software ISP)

The capture's worker thread runs the 3A loop over the stream. A still request reaches it
through the loop's controls; a `StillRunner` there decides which frames' raw data to keep:

* `Current`: the next frame (with `settle`: the first after AE reports locked).
* `Fixed` / `Bracket`: the application's controls are put aside and the loop is given a fixed
  exposure and gain per shot. The first request AE makes with them says which frame they land
  on (the frame-exact control schedule: issue latency + control delay); that frame, produced
  with those values (checked against what the frame reports, read back from embedded data where
  the sensor has it), is kept. The next shot's controls go out on the next frame, so a bracket
  lands on consecutive frames. After the last shot the application's controls (AE) come back.
  Bracket exposures are AE's total exposure × 2^EV, split as exposure time at AE's gain up to
  the frame's length, then gain.

On the PiSP the front end's 16-bit raw buffer of a wanted frame is copied while the back end
processes the preview (`PispPipeline::set_raw_copy`); on the software ISP path the receiver's
packed frame is copied after its preview frame is made. The copy goes with the settings that
processed the frame (white balance, CCM, lens shading, gamma, black level) and the algorithms'
state (colour temperature, lux) to a still thread; the preview never waits for it.

The still thread reprocesses at full quality:

* **PiSP**: the back end has two node groups; the preview uses group 0, stills group 1
  (`StillBackEnd`), a memory-to-memory job (as `native-pipeline be-replay`) with its own config:
  full size, output NV12 or RGB, spatial and colour denoise at `denoise` × the stream's
  thresholds, no temporal denoise (a single frame). If group 1 cannot be opened the software
  ISP takes over.
* **Software ISP**: `styx-softisp` with the Malvar-He-Cutler demosaic (the stream uses
  bilinear), on `soft_threads` threads.
* Bracketed and fixed-exposure stills keep their own exposure: the stream's digital gain
  (which makes up for AE's exposures still on their way) is replaced by the white balance's
  green gain.

JPEG comes from the codec registry's RG24 → MJPG encoder (mozjpeg, turbojpeg, FFmpeg), the
request's `encoder`, or the `image` crate (feature `image`, pure Rust).

### Other captures (V4L2, UVC, libcamera, files)

The still is the next frame the capture delivers, taken from its queue (so the consumer misses
that frame), at the running mode, converted to the format asked for (an MJPG frame is passed
through as the JPEG). Fixed and bracketed exposures and DNGs need a native camera. For a
full-resolution still from a V4L2 camera running a smaller mode, reconfigure the capture
(`CaptureHandle::reconfigure_in_place`) to its largest mode first. UVC still-image methods
(still probe/commit, the still trigger) are not implemented (TODO.md).

## DNG files

`styx_pipeline::still::dng_metadata` fills a `styx_dng::DngMetadata` for a held frame; `styx_dng::write_dng` writes it:

* IFD 0: the sRGB preview (8-bit RGB, uncompressed), camera profile tags; SubIFD: the raw image
  (16-bit samples at the sensor's depth, e.g. 0..1023, or 8-bit, uncompressed, one strip); EXIF
  IFD: exposure time, ISO (100 × sensor gain), capture time, a JSON user comment.
* `CFAPattern` 2×2, `BlackLevel` per CFA cell from the tuning (`BlackLevelRepeatDim` 2×2),
  `WhiteLevel` 2^bits − 1.
* `AsShotNeutral` = 1 / the white balance gains the frame was processed with.
* `ColorMatrix1/2` at standard illuminant A (2856 K) and D65 (6504 K), `ForwardMatrix1/2`,
  `CalibrationIlluminant1/2`, from the tuning (below).
* `BaselineExposure` = log2 of the ISP's digital gain, so a default rendering is as bright as
  the ISP's.
* `OpcodeList2`: lens shading as four `GainMap`s (one per CFA channel, `RowPitch`/`ColPitch` 2,
  the ALSC grid's cell centres as map points), flagged optional.
* `UniqueCameraModel` names the sensor and the tuning (`Styx ov9782 (builtin:ov9782.json)`).

### Colour matrices from the tuning

A Raspberry Pi / Styx tuning gives, per colour temperature T, a CCM `M(T)` (white-balanced
camera RGB → linear sRGB, rows summing to 1: the light's white goes to sRGB white, so it
includes the adaptation to D65) and, from the AWB curve, the camera's neutral `n(T)` = (R/G,
1, B/G) of a grey. The ISP computes `s = M · diag(1/n) · c`. With `A` the sRGB → XYZ (D65)
matrix and `B` Bradford's adaptation from D65 to the light's white `W`, the scene colour is
`XYZ = B · A · M · diag(1/n) · c`, and DNG's `ColorMatrix` (XYZ → camera) is its inverse:

```text
ColorMatrix(T)   = k · diag(n) · M⁻¹ · A⁻¹ · B⁻¹,   k: largest component of ColorMatrix · W = 1
ForwardMatrix(T) = Bradford(D65 → D50) · A · M,     rows scaled so (1,1,1) ↦ D50 white
```

`styx_dng::color` implements it (`color_matrix`, `forward_matrix`) and the inverse
(`ccm_from_color_matrix`: a `ColorMatrix` and its illuminant back to the CCM and neutral, for
calibration from DNGs); tests check that the light's white maps to the neutral and that the
round trip gives the CCM back. A tuning without an AWB curve gets one calibration from the
shot itself (its CCM and neutral at the D65 slot). The OV9782 tuning's CCM is the identity
(the HeliOS tuning), so its DNGs carry identity-CCM matrices: the colour is the ISP's.

### The reader

`styx_dng::read_dng` reads this writer's files and camera DNGs: TIFF in either byte order,
the raw IFD wherever it is (IFD 0, SubIFDs, chained IFDs), strips or tiles, uncompressed
samples at any depth (8, 16, or packed MSB-first) or lossless JPEG (`Compression` 7, all seven
predictors, any component count, restart markers), linearization tables, CFA layout, black
level (repeat pattern, row/column deltas, active area) and white level, as-shot neutral or
white, analogue balance, colour/forward/camera calibration matrices with illuminants,
baseline exposure, exposure time, ISO, F-number, names, date, and all three opcode lists
(`GainMap` decoded, others kept as bytes). Not read: lossy JPEG and deflate (floating point)
DNGs. Every offset and count is checked; damaged files are errors, not panics.

## Validation

* Adobe's `dng_validate` (DNG SDK): not available on the development host (no SDK installed;
  system packages were not to be added), so not run.
* Round trips (`cargo test -p styx-dng`): every tag the writer writes reads back; matrices back
  to the tuning's CCM within 2e-3 (file precision 1e-4); camera-style files built in the tests
  (big-endian, lossless-JPEG tiles of two components, 12-bit packed strips, linearization
  table) decode to their samples.
* Tools on the host: `exiv2` lists every tag (DNG version 1.4, backward 1.3 with opcodes,
  SubIFD raw, CFA, black/white levels, matrices, illuminants, EXIF); LibRaw 0.22.2 (through
  ImageMagick 7.1.2: `magick -define dng:use-camera-wb=true -define dng:no-auto-bright=true
  still.dng still.png`) renders every file Styx wrote on the CM5 (PiSP and software ISP stills,
  brackets, a recording's frame). Against the ISP's JPEG of the same frame (PiSP, 30 fps):
  mean R/G 1.44 vs 1.34 and B/G 1.10 vs 1.09 (same white balance and colour); LibRaw's render
  is darker (its plain sRGB curve against the tuning's contrast curve, no lens shading: LibRaw
  does not apply `GainMap`s). The bracket renders step with the exposure (LibRaw green mean
  77 / 114 / 163 for −1 / 0 / +1 EV; the ISP's 107 / 162 / 207).

## Measured on the CM5

OV9782 1280x800 through the bridge, NV12 preview through `Frames`, `still_capture` (release
build, 2026-10-03), JPEG by the `image` crate:

| | PiSP, 30 fps | PiSP, 60 fps | software ISP, 30 fps |
|---|---|---|---|
| preview while taking stills | 30.00 fps, 0 sequence gaps, longest interval 33.3 ms | 59.98 fps, 0 gaps, 16.7 ms | 30.00 fps, 0 gaps, 33.3 ms |
| still (AE locked, JPEG + DNG), request → ready | 95-134 ms | 85 ms | 95-133 ms |
| of that, reprocess + JPEG + DNG on the still thread | 44-49 ms | 44-49 ms | 45-51 ms |
| 3-shot bracket (−1, 0, +1 EV) request → ready | 335-353 ms | 250 ms | 360-401 ms |
| bracket frames (target = frame, `landed`) | 84, 85, 86 (all landed) | 160, 161, 162 | 85, 86, 87 |

At +1 EV and 30 fps the exposure is capped by the frame (33.2 ms) and the rest goes to gain
(×2.19). The V4L2 path (Logitech C270, YUYV 640x480): request → JPEG 68 ms; the preview misses
the frame taken (one sequence gap).

# Algorithms (`styx-algo`)

Camera control algorithms ("3A") in Rust: automatic exposure, white balance, lens shading,
colour and tone, driven by tuning data, deterministic and replayable. `crates/algo` has no
dependency on other Styx crates; the session runtime converts between it and `styx-sensor`
(register codes, delays) and the ISP (statistics, parameters).

## Licensing and provenance

| Source | License | Use |
|---|---|---|
| Raspberry Pi IPA, libcamera `src/ipa/rpi/controller/**` (`agc_channel.cpp`, `awb.cpp`, `awb_bayes.cpp`, `alsc.cpp`, `ccm.cpp`, `contrast.cpp`, `black_level.cpp`, `lux.cpp`, `histogram.cpp`) | BSD-2-Clause (SPDX header in every file), Copyright Raspberry Pi Ltd | Ported, with a provenance comment on each module |
| libcamera `src/ipa/libipa/pwl.cpp` | BSD-2-Clause, Raspberry Pi Ltd / Ideas on Board | Semantics of `Pwl` (written anew) |
| Raspberry Pi tuning files `src/ipa/rpi/{pisp,vc4}/data/*.json` | BSD-2-Clause (libcamera `REUSE.toml`) | Format read by the loader; imx219 CT curve, priors and CCMs used as simulator and test data (marked in the files) |
| libcamera core and most of `libipa` | LGPL-2.1-or-later | Not used |
| HeliOS tuning (`gaia/assets/libcamera/ipa/rpi/*/ov9782.json`) | HeliOS repository is GPL-2.0; the file says it is derived from Raspberry Pi's imx219 tuning | Only read by a test at run time, never copied |

## Framework

```text
Statistics ─┐
FrameMetadata (exposure, gain, frame duration, lux?, Controls) ─┤
            └─► Pipeline: black_level → lux → awb → agc → alsc → ccm → contrast ─► Params
```

* `Statistics` (`stats.rs`): colour zones (sums of R, G, B normalised to full scale 1.0, black
  level removed, plus pixels counted), optional luma zones, a luma histogram, optional focus
  values, and whether colour statistics were taken before white balance / lens shading.
  PiSP: divide each sum by `2^bits` of that statistic; AWB regions → `colour`, AGC regions →
  `luma`, Y histogram → `histogram`, CDAF → `focus`. A software ISP adds pixels to a
  `StatsAccumulator`.
* `FrameMetadata` (`frame.rs`): what produced the frame (from `ControlScheduler::applied`,
  converted to durations and ratios) and the application `Controls` in effect (AE/AWB enable,
  fixed exposure/gain, EV in stops, metering/exposure/constraint/AWB mode names, flicker,
  frame duration limits, manual gains or temperature, saturation, brightness, contrast).
  Controls travel with each frame so recordings reproduce control changes.
* `CameraConfig` (`config.rs`): the sensor mode's exposure, frame duration and analogue gain
  limits, exposure margin, `ControlDelays` (exposure, gain, frame length, and the issue latency),
  sensitivity, sensor black level, crop and flips.
* `Params` (`params.rs`): `SensorRequest { frame, exposure, analogue_gain, frame_duration }`,
  digital gain, colour gains and temperature, CCM, tone curve (`Pwl` on `[0, 1]`), black
  levels, lens-shading tables, `AeStatus`, `AwbStatus`, lux. Params persist from frame to frame
  and are how algorithms pass results on (CCM reads the temperature AWB wrote).
* `Algorithm`: `prepare(&CameraConfig)` resets state for a mode, `initial(&mut Params)` gives
  start-up values, `process(&Statistics, &FrameMetadata, &mut Params)` runs per frame.
* `Pipeline`: an ordered list over one `Params`; `Pipeline::from_tuning` builds the standard
  set. Same inputs → bit-identical outputs.

### Frame timing

`SensorRequest::frame` is `F + issue_latency + max(delays)` for statistics of frame `F`: the
first frame on which exposure, gain and frame length can all land together. Hand it to
`ControlScheduler::request(frame, ..)` (or `request_now`, which writes what is due in the
current frame at once), which writes each control `delay` frames earlier. `issue_latency` is 0
when requests are written in the frame the statistics came from (the PiSP path at 30 and 60
fps), 2 when they wait for the start of `F + 2`. A request equal to the previous frame's means
nothing new (it keeps its frame). AE's targets are total exposures computed from what produced
each frame, so a frame still in flight asks for the same total again and does not re-trigger a
change; only a clearly different one (more than `full_step`) replaces it.

`CameraConfig::unsettled_frames` (from the sensor description's black level `settle_frames`):
frames at the start of a stream whose levels are not reliable yet; AE and AWB leave them out.

### Warm starts

`WarmStart` is what AE and AWB settled on: AE's total exposure with the mode's sensitivity, the
exposure and gain that delivered it, AWB's gains and temperature, lux, and whether they were
locked/converged. `Pipeline::warm_state` takes it from the latest parameters,
`Pipeline::prepare_warm(config, Some(&warm))` starts from it (`Algorithm::warm_start`, default:
ignore): AGC re-splits the total along the exposure profile within the new mode's limits (so a
30 → 120 fps switch keeps the brightness with more gain), AWB starts at the gains. Replays
record the warm start in their header.

## Algorithms

| Algorithm | Source | Notes |
|---|---|---|
| `BlackLevel` | `black_level.cpp` | Tuning levels, else the sensor description's |
| `Lux` | `lux.cpp` | From exposure, gain and mean luma against a reference; metadata lux wins |
| `Agc` | `agc_channel.cpp` (channel 0) | Metering (tuned or built-in centre-weighted / spot / average weights, resampled to the zone grid), histogram constraints, EV, exposure profiles, flicker periods, digital gain, damping, start-up, fast de-saturation, lock. Styx changes: output landing frame; frame duration chosen here; model-based steps (changes above `full_step`, 8% by default, go straight to the target at any time; after one lands, the rest is corrected at once; damping only for small changes); every frame's statistics used (frames in flight ask for the same total); de-saturates only while half the image is saturated and without damping; locked = no change beyond 5% in flight and on target (5%) for two frames, no hunting within that tolerance once locked; unsettled frames left out; warm starts |
| `Awb` | `awb.cpp`, `awb_bayes.cpp` | Bayesian search along the CT curve with lux-interpolated priors, coarse then fine (across the curve), or grey world; runs synchronously every `frame_period` frames (every frame during start-up), filtered by `speed`; modes; manual gains or temperature. Styx changes: start-up counts only usably exposed frames (mean luma 0.02..0.7; at most 4 × `startup_frames` frames); unsettled frames left out; warm starts |
| `Alsc` | `alsc.cpp` | Calibrated Cr/Cb tables interpolated by temperature, resampled to crop and flips, normalised, luminance table at `luminance_strength` (tuned or generated from `corner_strength`). The adaptive refinement is not ported |
| `Ccm` | `ccm.cpp` | Interpolated by temperature, saturation control and saturation-by-lux |
| `Contrast` | `contrast.cpp` | Gamma curve, adaptive histogram stretch, manual brightness/contrast |

## Tuning

Our format is TOML matching `Tuning` (`tuning/mod.rs`): sections `[black_level]`, `[lux]`,
`[agc]`, `[awb]`, `[alsc]`, `[ccm]`, `[contrast]`; unknown keys are rejected, missing keys
take the defaults (an empty file is a working grey-world, centre-weighted tuning). Levels are
normalised to 1.0, times are microseconds (`*_us`), temperatures kelvin.
`crates/algo/tests/data/sim.toml` is an example. `Tuning::load(path)` reads `.json` as a
Raspberry Pi file and anything else as TOML; `to_toml_string` converts.

Raspberry Pi tuning files (version 2) convert with `Tuning::from_rpi_json_str`, which also
lists what it did not use (`RpiImport::ignored`). The reader accepts trailing commas and keeps
key order (the first mode listed is the default), as libcamera's YAML-based reader does.

| Raspberry Pi | styx-algo |
|---|---|
| `rpi.black_level.black_level[_r/_g/_b]` (16-bit) | `black_level.r/g/b` ÷ 65536 |
| `rpi.lux.reference_shutter_speed`, `_gain`, `_aperture`, `_lux`, `reference_Y` | `lux.reference_exposure_us`, …, `reference_y` ÷ 65536 |
| `rpi.agc` or `rpi.agc.channels[0]` | `agc` (other channels and `channel_constraints` ignored) |
| `metering_modes.<m>.weights` (first = default) | `agc.metering_modes.<m>.weights`, `default_metering_mode`; grids inferred (15×15 on PiSP); the old 15-region VC4 layout falls back to built-in weights |
| `exposure_modes.<m>.shutter` / `gain` | `agc.exposure_modes.<m>.exposure_us` / `gain` |
| `constraint_modes.<m>[] { bound, q_lo, q_hi, y_target }` | same, `bound = "lower"/"upper"` |
| `y_target`, `speed`, `startup_frames`, `convergence_frames`, `fast_reduce_threshold`, `base_ev`, `default_exposure_time`, `default_analogue_gain`, `stable_region`, `desaturate`, `max_digital_gain` | same names (`default_exposure_us`); `full_step` is Styx's own (default 0.08) |
| `rpi.awb.ct_curve` (flat triples) | `awb.ct_curve = [[ct, r, b], …]` |
| `priors[] { lux, prior }`, `modes` (first = default), `bayes`, `min_G` (16-bit), `min_pixels`, `min_regions`, `coarse_step`, `whitepoint_r/b`, `bias_proportion`, `bias_ct`, `delta_limit`, `transverse_pos/neg`, `sensitivity_r/b`, `speed`, `frame_period`, `startup_frames` | same names; `min_g` ÷ 65536 |
| `rpi.alsc.calibrations_Cr/Cb`, `luminance_lut`, `corner_strength`, `asymmetry`, `luminance_strength`, `default_ct` | `alsc.calibrations_cr/cb`, …; `grid` from the table size (1024 → 32×32, 192 → 16×12). `omega`, `n_iter`, `sigma*` (adaptive part) ignored |
| `rpi.ccm.ccms[] { ct, ccm }`, `saturation` | `ccm.ccms`, `ccm.saturation` |
| `rpi.contrast.gamma_curve` (16-bit x, y), `lo_*`, `hi_*`, `ce_enable` | `contrast.gamma_curve` ÷ 65535, `lo_max`/`hi_max` ÷ 65536 |
| `rpi.noise`, `rpi.denoise`, `rpi.sharpen`, `rpi.dpc`, `rpi.geq`, `rpi.hdr`, `rpi.af`, `rpi.cac`, `rpi.sdn`, `rpi.sync`, `rpi.nn.awb`, … | ignored (not implemented yet) |

All 67 pisp and vc4 tuning files in libcamera and both HeliOS OV9782 files convert and run.

## Adding an algorithm

1. Add `src/algos/<name>.rs` (or a directory) with a tuning struct deriving
   `Serialize, Deserialize` with `#[serde(deny_unknown_fields, default)]`, a `Default` that
   works for an unknown camera, and `validate()`.
2. Implement `Algorithm`: reset all state in `prepare`, write start-up values in `initial`,
   and in `process` read inputs and earlier algorithms' results from `Params`, then write
   yours. Keep it deterministic: no clocks, threads, randomness, or `HashMap` iteration.
   Add output fields to `Params` if needed.
3. Add the section to `Tuning` (and `validate`), place it in `Pipeline::from_tuning`, and map
   the Raspberry Pi section in `tuning/rpi.rs` if there is one (list its known keys so the
   rest is reported as ignored).
4. When porting, port only BSD (or similarly permissive) code and say where it came from in the
   module comment; otherwise write it from first principles.
5. Test: unit tests with synthetic `Statistics`, a simulator test (`sim`) for anything with
   dynamics, and check replays stay bit-identical.

## Simulator and replay

`sim::Simulation` models a scene (lux and colour temperature over frames, mains flicker,
reflectances), a sensor (CT response, responsivity, shot and read noise, texture, line-quantised
exposure) and the control scheduler's timing (per-control delays, requests from frame `F`
written from `F + 2`, late landings counted). `sim::convergence` measures settle frames,
overshoot and jitter. `SensorModel::black_error` adds a black level offset (luma not
proportional to exposure). Results with the default 30–10 fps mode, delays 2/1/2, issue
latency 2 (`tests/sim_ae.rs`), and with the OV9782's timing as the PiSP path drives it at 30
fps (delays 2/2/1, written in the same frame: `tests/sim_start.rs`), run with `--nocapture`:

| Case | Result (issue latency 2) | Result (same-frame writes) |
|---|---|---|
| AE start-up at 20 lux (from 1 ms) | within 5% after 8 frames (was 8) | after 4, locked at 5 |
| AE start-up at 400 lux | | after 2, locked at 3 |
| AE 20 → 5000 lux (saturated) | within 5% after 20 frames (was 26), overshoot 0.2%, jitter 0.12% | after 10, locked after 11 |
| AE 5000 → 20 lux | within 5% after 8 frames (was 16), overshoot 0.3% | |
| AE 200 → 300 lux | within 3% after 4 frames (was 11, damped) | |
| AE 200 → 214 lux | within 3% after 4 frames (damped) | |
| AE 800 → 200 / 200 → 800 lux | | after 2 / 6, locked after 3 / 7 |
| AE 200 → 50 lux with a black level error of 0.01 | | after 4, locked after 5 |
| warm restart, same scene | | within 5% from frame 0, locked at 1, nothing re-requested |
| 30 → 120 fps warm start | | 30 ms × 2.1 → 8.1 ms × 7.8, luma within 0.3%, locked at 1 |
| 100 Hz flicker, 20 ms exposure | frame-to-frame jitter 2.1% without avoidance, 0.13% with 50 Hz avoidance |
| AWB 3000 K → 6000 K / 6000 K → 3000 K | 6004 K / 3007 K, gains within 0.1% of truth, settled (3%) in 54–64 frames at the default `speed` 0.05 |
| Late landings | none |

`replay`: JSON Lines, a header `{"styx_algo_replay": 1, "config": …}` then one
`{"stats", "meta", "params"?}` per frame. `Recorder` writes, `Recording::read` loads,
`replay::replay` reruns a pipeline and reports frames whose output differs from the recorded
one. Floats round-trip exactly.

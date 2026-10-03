# Algorithms (`styx-algo`)

Camera control algorithms ("3A") in Rust: automatic exposure, white balance, lens shading,
colour and tone, driven by tuning data, deterministic and replayable. `crates/algo` has no
dependency on other Styx crates; the session runtime converts between it and `styx-sensor`
(register codes, delays) and the ISP (statistics, parameters).

## Licensing and provenance

| Source | License | Use |
|---|---|---|
| Raspberry Pi IPA, libcamera `src/ipa/rpi/controller/**` (`agc_channel.cpp`, `awb.cpp`, `awb_bayes.cpp`, `alsc.cpp`, `ccm.cpp`, `contrast.cpp`, `black_level.cpp`, `lux.cpp`, `histogram.cpp`, `af.cpp`), `src/ipa/rpi/cam_helper/cam_helper_imx708.cpp` (PDAF decoding, in `styx-sensor`) | BSD-2-Clause (SPDX header in every file), Copyright Raspberry Pi Ltd | Ported, with a provenance comment on each module |
| libcamera `src/ipa/libipa/pwl.cpp` | BSD-2-Clause, Raspberry Pi Ltd / Ideas on Board | Semantics of `Pwl` (written anew) |
| Raspberry Pi tuning files `src/ipa/rpi/{pisp,vc4}/data/*.json` | BSD-2-Clause (libcamera `REUSE.toml`) | Format read by the loader; imx219 CT curve, priors and CCMs used as simulator and test data (marked in the files) |
| libcamera core and most of `libipa` | LGPL-2.1-or-later | Not used |
| HeliOS OV9782 PiSP tuning (`ipa/rpi/pisp/ov9782.json`, HeliOS before 6f9832f, now in the Atlas Raze device package) | The project owner's own tuning, cleared by them for use in Styx (2026-10-02); built on Raspberry Pi's tuning format and data (BSD-2-Clause) | Embedded unchanged in `styx-pipeline` (`tuning/ov9782.json`, md5 f06d0ac9…) as the OV9782's built-in tuning |

## Framework

```text
Statistics ─┐
FrameMetadata (exposure, gain, frame duration, lux?, Controls) ─┤
            └─► Pipeline: black_level → lux → awb → agc → alsc → ccm → contrast → denoise → af ─► Params
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
| `Agc` | `agc_channel.cpp` (channel 0) | Metering (tuned or built-in centre-weighted / spot / average weights, resampled to the zone grid), histogram constraints, EV, exposure profiles, flicker periods, digital gain, damping, start-up, fast de-saturation, lock. Styx changes: output landing frame; frame duration chosen here; model-based steps (changes above `full_step`, 8% by default, go straight to the target at any time; after one lands, the rest is corrected at once; damping only for small changes); every frame's statistics used (frames in flight ask for the same total); de-saturates only while half the image is saturated and without damping; locked = no change beyond 5% in flight and on target (5%) for two frames, no hunting within that tolerance once locked, and locked at its limits when the scene is beyond them (`AeStatus::at_limit`); unsettled frames left out; warm starts; flicker fitted from the frames and metered against, detected automatically, and taken out of the frames with per-frame ISP gain (below); on target compares what the frame asks for with AE's own total (the sensor's share may be smaller: digital gain, deflicker headroom) |
| `Awb` | `awb.cpp`, `awb_bayes.cpp` | Bayesian search along the CT curve with lux-interpolated priors, coarse then fine (across the curve), or grey world; runs synchronously every `frame_period` frames (every frame during start-up), filtered by `speed`; modes; manual gains or temperature. Styx changes: start-up counts only usably exposed frames (mean luma 0.02..0.7; at most 4 × `startup_frames` frames); unsettled frames left out; warm starts; soft search and hysteresis (below) |
| `Denoise` | `noise.cpp`, `denoise.cpp`, `geq.cpp`, `dpc.cpp`, `sharpen.cpp` | Noise profile × √analogue gain; SDN/CDN/TDN strengths (`normal` configuration), SDN and CDN starting at their no-TDN values and backing off by `backoff` per run while temporal denoise runs (`CameraConfig::temporal_denoise`, set by the ISP); GEQ by gain (and lux); DPC strength; sharpening factors. Output in `Params::denoise` / `Params::sharpen` (Raspberry Pi units: 16-bit pixel scale) for ISPs with those blocks (the PiSP back end) |
| `Alsc` | `alsc.cpp` | Calibrated Cr/Cb tables interpolated by temperature, resampled to crop and flips, normalised, luminance table at `luminance_strength` (generated from `corner_strength` if given, else tuned). Adaptive refinement (below). Styx changes: the refinement runs in `process` and its result is used from the next frame on (libcamera: a thread, picked up a frame or more later); the filter and periods count frames, so running the algorithms at a lower rate keeps the per-frame speed |
| `Agc` → ISP | `pisp.cpp` (`setHistogramWeights`) | AGC also writes `Params::histogram_weights`, the metering mode's weights on the tuning's grid (15×15), for ISPs that weight their luma histogram by zone: the PiSP front end, programmed as the Raspberry Pi IPA does |
| `Ccm` | `ccm.cpp` | Interpolated by temperature, saturation control and saturation-by-lux |
| `Contrast` | `contrast.cpp` | Gamma curve, adaptive histogram stretch, manual brightness/contrast |
| `Af` | `af.cpp` | PDAF loop and CDAF scans driving a focus lens in dioptres; always in the pipeline, inert without a lens (`CameraConfig::lens`). Styx changes below |

### ALSC: adaptive refinement

The calibrated tables correct the lens; what is left (a calibration made with another lens
unit, or under another light) shows as colour drifting across uniform surfaces. Every
`frame_period` frames (every frame for `startup_frames`), with the calibration for the
current temperature folded into the 32×32 zones' R/G and B/G, ALSC solves for red and blue
gains `λ` that make neighbouring zones of similar colour (weight `exp(-((C_i - C_j)/σ)²/2)`)
come out equal: Gauss-Seidel sweeps (forwards and backwards) with over-relaxation `omega`,
each gain within `1 ± lambda_bound`, until no gain moves by `threshold` or after `n_iter`
sweeps. The final red table is `λ_r × calibration_r` normalised to a minimum of 1, times the
luminance table (blue the same, green the luminance table alone); the tables in use move
towards each new result by `speed` per frame (at once during start-up). Gains persist across
runs (and across restarts in the same mode), so the estimate builds up.

Checked against the original: its iteration functions compiled on the host with stand-ins for
its array types give the same gains to 1e-12 on three 32×32 cases
(`tests/data/alsc_gs_reference.json`). Two of the original's helpers do nothing (`reaverage`
and the final `normalise`: their `std::for_each` lambdas return the value instead of assigning
it); the port leaves them out, so it matches what libcamera actually runs. Statistics on
another grid than the tables' (the software ISP's 16×12 zones with a 32×32 tuning) leave the
tables at the calibration. Cost: a run from scratch (both channels, 20 sweeps) takes 0.24 ms on
a desktop x86 core; see "Quality vs libcamera" in [pipeline.md](pipeline.md) for the CM5.

### AWB: continuity and hysteresis

Under a lamp below the CT curve's range (the OV9782's room: about 2250 K, the curve starts at
2860 K, the `auto` mode at 2500 K) every zone is capped (`delta_limit`) both at the low end of
the search and at the 6500 K peak of the lux prior, and the two costs differ by a fraction of
one log-likelihood unit; the frame-to-frame noise of that difference is about ±1.5 on the
software ISP's 16x12 zones. Raspberry Pi's search takes the best point, so the estimate
jumped between ~2500 K and ~6500 K (a 0.07% scaling of the statistics, which moves the lux
estimate and so the prior, was reported to flip it from 2533 K to 4463 K; a fresh estimate
of the recorded frame 80 swept over scalings jumps from 2533 K to 6493 K at one point; in a replay of the recorded
frames every 10-frame estimate could flip). libcamera has the same search, damped only by
`speed`. Styx's search now:

* weights the coarse points by `exp(-(cost - best) / softness)` (default 0.2) and takes their
  mean in mired, and weights the fine search's steps along the curve the same way, so the
  estimate is a continuous function of the statistics; with one clear minimum this is the
  minimum (softness 0 is the original search);
* adds a hysteresis well around the best coarse point of the last estimate made on a usable
  frame (`hysteresis` 2 log-likelihood units deep, `hysteresis_mired` 25 wide): another
  temperature has to fit better by about 2 before the estimate moves there. The first usable
  estimate is free and start-up still applies estimates at once, so the initial convergence
  is unchanged (simulation: 3000 ↔ 6000 K settle in 54-64 frames as before, ending at 2999 /
  5994 K); a real change of the light (tens of units) moves at once.

On the recorded replay every scaling between 0.9 and 1.1 now stays at 2440 ± 10 K after
start-up (was: 2533 K with 10-frame excursions to 6493 K). `tests/awb_continuity.rs` sweeps
the statistics' scale on a synthetic tie and on the recorded tie (`tests/data/awb-tie.jsonl`,
with the libcamera tree's OV9782 tuning when present) and requires steps under 6 mired per
0.1% (under 1 mired per 0.01% around the old flip), and checks that frames alternating
either side of the tie keep the side chosen.

### AE: flicker

Raspberry Pi's AGC avoids mains flicker by making exposures whole flicker periods (10 ms for
50 Hz mains), which needs exposures of at least a period: at 120 fps (8.1 ms at most) it does
nothing, and a global-shutter sensor's frames beat at the alias frequency (100 Hz at 120 fps:
20 Hz; a lamp flickering at 50 Hz: 50 Hz), which AE chased (on the CM5 at 120 fps under the
room's lamp: exposure × gain spread 10%, never locked). Styx's AGC (`agc/flicker.rs`):

* models the light as harmonics 1-3 of the mains frequency (50, 100, 150 Hz), so lamps
  flickering at the mains frequency itself (half-wave LED drivers, the OV9782's room) are
  covered as well as the usual full-wave 100 Hz; a frame sees each harmonic integrated over its
  exposure (`sinc(kωT/2)` of it, at the exposure's centre);
* fits the model by least squares (QR by Gram-Schmidt) to the last second of frames (at most
  64): each frame's mean luma over its exposure × gain, so AE's own changes drop out, at times
  from the frame numbers and durations (frames the algorithms skip are fine). Phases are
  relative to the newest frame, and a strongly significant fit (F ≥ 30) tracks the mains
  frequency: the residual at ±0.05 Hz, a parabola through the three, 30% of the step to its
  minimum per frame (within ±0.4 Hz of nominal; the trials keep the fit's harmonics, so two
  that alias onto each other, 50 and 100 Hz at 30 fps, are not told apart by drift and noise).
  The room's 50 Hz measured 50.07 Hz in the frames' clock: untracked, a prediction half a
  window ahead is 13° off at 50 Hz, 38° at 150 Hz. Harmonics the
  frames cannot see (aliased to a constant phase, e.g. 120 Hz at 120 fps; whole periods of
  exposure) or cannot tell apart from one already in (50 and 100 Hz both at ±10 Hz at 30 fps;
  the second harmonic goes in first) are left out; a frame far from the model (25%) is a scene
  change and restarts the window;
* meters each frame against the mean light: the metered gain is multiplied by the brightness
  the model predicts for that frame (`AeStatus::flicker_modulation`), when the fit is
  significant (F test of the flicker terms ≥ 12, modulation ≥ 0.4%);
* quantises exposures of at least a period to whole periods: the mains period (20 ms) once
  the lamp has been seen flickering at the mains frequency (kept: such exposures hide it from
  the fit), else half of it (10 ms; also what `Flicker::Mains50` uses before a fit). Shorter
  exposures are left alone (10 ms ones would not cancel a 50 Hz component, and longer ones see
  less of it);
* `Flicker::Auto` fits 50 and 60 Hz mains and takes the one whose fit stays strongly
  significant (F ≥ 30, modulation ≥ 0.8%, 16 frames or more) for half a second; until then AE
  meters against the most significant fit, and exposures are quantised only once detected.
  The detection is kept (also across warm starts, `WarmStart::flicker_detected`) until the
  other frequency is detected. Styx's native processed modes default to it
  (`NativeIspConfig::flicker`, control `AE_FLICKER_MODE`, detected period
  `AE_FLICKER_DETECTED`); `Controls::default()` keeps it off.

### Deflicker

Exposures shorter than a period keep the flicker in the frames: AE meters against the mean
light, but the frames still beat (the room's lamp: 8-12% output spread at 120 fps). The same
model predicts each frame's brightness from its exposure window, so the ISP can divide it out
(`agc/deflicker.rs`, `Params::deflicker`: a `FlickerCorrection`, and `Params::frame_gain`):

* **The right frame.** The correction carries the model, its reference time and the clock of
  the frame AGC last ran on; the ISP settings for frame F (made from F − 1 on the PiSP path,
  from further back at the settled rate) ask it for F's brightness with F's own number,
  duration and exposure (`Controller::retarget`). A one-frame lag would double the flicker
  instead (unit test: 13% → 25%).
* **One gain per frame, or per band.** A global shutter (OV9782) exposes all rows together.
  For a rolling shutter (`CameraConfig::readout` > 0) `FlickerCorrection::band_gains` gives
  each band of rows its own gain (the fit saw the frame mean, so the harmonics are first
  divided by the readout's `sinc`); the pipeline folds them into the lens shading grid, which
  both the PiSP and the software ISP apply; their mean effect is 1 so the statistics still see
  the frame's flicker. Unverified on a sensor (none here is rolling shutter).
* **Confidence.** Only a fit with F ≥ 30, 16 frames or more sampled on most frames (a fit from
  the settled rate's every eighth frame can alias the harmonics onto each other) and a
  modulation of 1% or more turns it on; it fades in and out over 0.25 s. The coefficients are
  low-pass filtered (30% per frame) after rotating the previous ones to the new reference
  time. While the light is seen flickering `Params::needs_every_frame` keeps the algorithms
  off their settled rate (the fit needs every frame); in steady light nothing changes.
* **Headroom, highlights and noise.** A frame brighter than the mean needs a gain below 1.
  Where the frames' highlights (the luma histogram's 99.9% quantile, ×1.25 for colour
  channels, on the output's scale under the mean light) stay clear of clipping even in the
  brightest frame, gains below 1 are harmless and the sensor keeps its exposure. Where they
  would not, AE leaves as much of its total exposure to the ISP's digital gain as the
  highlights need (up to the brightest frame: `FlickerCorrection::headroom`), and no frame
  gets less than `headroom / peak`: what clips in the raw frame stays white. The need follows
  the largest of the last second or so (decaying 10%/s), the headroom moves only when the
  need is 3% above or 10% below it, and AE's lock does not count a headroom change as a
  change of exposure. Gains stay within 1/1.6..1.6.
* **Hand-off.** Exposures of whole periods see none of the flicker (`sinc = 0`): their
  predicted brightness is 1, so switching between short and quantised exposures needs
  nothing; without a confident fit the correction fades out and the headroom returns to 1.
* **Controls.** `Controls::deflicker`: `Deflicker::Off`, `On` (fits 50 and 60 Hz as
  `Flicker::Auto` does when avoidance is off, without quantising exposures) or `Auto` (the
  default: on when flicker avoidance is). Off while AE is (manual exposure and gain).

Simulated (`tests/sim_deflicker.rs`, the device's timing, the room's lamp on mains 0.07 Hz off
nominal, output = raw mean × the gain from the frame before's parameters as on the PiSP path,
steady state over seconds 3-10):

| | deflicker off | deflicker auto |
|---|---|---|
| 120 fps (8.1 ms): output SD | 13.3% | 0.12% (gains 0.83..1.19, no headroom, no AE change) |
| 90 fps (10 ms) | 11.3% | 0.12% |
| 60 fps (8.3 ms) | 13.2% | 0.12% |
| 120 fps, a clipped lamp in view | 9.9% (AE never locks: chases the beat, also before deflicker) | 0.12%, headroom 1.26, gains ≥ 1.06 |
| 90 fps, 60.1 Hz lamp, noisy sensor (steady-light noise 1.08%) | raw 7.7% | 0.41% |
| 30 fps, 1.8 ms exposures (50 and 100 Hz alias 0.09 Hz apart) | 14.2% | 1.8% |
| 30 fps, whole 20 ms periods | 0.11% | 0.11% (nothing to correct) |
| steady light | | identical output, nothing enabled |

The residual 0.12% is the simulated frames' own noise. At 30 fps with short exposures (bright
scenes) 50 and 100 Hz alias onto frequencies 0.09 Hz apart, which a second of frames cannot
separate; the fit takes them as one and a few percent remain.

Simulated (`tests/sim_flicker.rs`, the device's timing): under 100 Hz light whose beat puts
±7% (±12%) on 8.1 ms frames at 120 fps, AE without avoidance moves exposure × gain by 5.4%
(7.8%) frame to frame and never locks; with avoidance it does not move after locking. Under
the room's half-wave lamp (50 Hz ±25%, 100 Hz ±10%, 150 Hz ±3%): 13% (off) against 0 (auto)
at 120 fps, 12% against 0 at 60 fps; at 30 fps auto ends at 20 ms exposures and the frames'
spread falls from 4.3% to 0.11%. 120 fps cold starts over 12 flicker phases (±3.5% beat):
locked at frames 5-14 and once 77 without, 5-11 with. Detection (mains 0.07 Hz off nominal):
frames 33 / 46 / 77 / 63 at 30 / 60 / 120 / 90 fps (60 Hz at 90 fps); none for steady light
or for 120 Hz light at 120 fps. Long runs (20-30 s, noise, mains 0.03 Hz off) with avoidance
never move exposure × gain after start-up. The device: pipeline.md, "Flicker".

### AF: autofocus

`Af` (`algos/af/`) drives a focus lens (a voice-coil motor) for cameras that have one
(`CameraConfig::lens`: the driver position range, the frames from a write to the frame it is
for, and the lens's own dioptre map). It works in dioptres (1 / distance in metres; 0 is
infinity) and turns them into driver positions with the tuning's `map` (else the lens's,
else a straight line from the normal range's far end at the lowest position to its near end
at the highest: a guess for an uncalibrated VCM).

Inputs: `Statistics::focus` (a figure of merit per zone, larger when sharper: the PiSP's 8×8
CDAF grid, or the software ISP's 16×12 green-gradient energy with a noise floor,
`StatsConfig::focus`), `Statistics::pdaf` (phase and confidence per cell, the IMX708's 16×12
from its embedded data), the colour zones (scene changes, the infrared test),
`FrameMetadata::lens` (where the lens was for the frame, `LensState`) and the controls.
Outputs: `Params::lens` (a `LensRequest`: driver position and the frame it is for) and
`Params::af` (`AfStatus`: mode, state, the lens position in dioptres, contrast, phase).

Controls (`Controls`, recorded per frame so replays reproduce them): `af_mode` (`Manual`:
`lens_position` in dioptres, the tuning's default position until one is given; `Auto`: one
scan per trigger; `Continuous`), `af_trigger` / `af_cancel` (counters: a change starts or
cancels a scan in auto mode), `af_range` (normal, macro, full), `af_speed` (normal, fast),
`af_windows` (up to 10 rectangles as fractions of the output, with weights; empty: the middle
half of the width and third of the height). States: idle, scanning, focused, failed.

The method is Raspberry Pi's: with phase data of enough confidence a feedback loop moves the
lens by `phase × pdaf_gain` per frame (slew limited by `max_slew`), for `pdaf_frames` frames
when triggered (ending early once the phase is small), all the time in continuous mode (small
moves squashed, cubically, below `pdaf_squelch`). Without phase data (or when it drops out for
`dropout_frames`), a contrast scan: coarse steps of `step_coarse` dioptres (from the near or far
end when triggered; in continuous mode in both directions from where the lens is) until the
contrast falls below `contrast_ratio` of its peak, a parabola through the peak and its
neighbours, a fine scan in `step_fine` steps back over it, a second parabola; two PDAF samples
during a scan can end it early by interpolating the zero-phase position. Continuous mode
without PDAF scans again after a scene change (contrast or the windows' colour moving by more
than `retrigger_ratio`) once the scene has been still for `retrigger_delay` frames.

Styx changes:

* **Frame-exact lens moves and reported positions.** A move is a `LensRequest` for a frame;
  the lens control writes it at the start of `frame − delay` (or at once when that has
  passed), as the control schedule does for exposure, and every frame reports where the lens
  was during its exposure, predicted from the moves and the lens's settle time (VCMs report
  no position). A scan step is measured on the first frame exposed with the lens settled
  there; libcamera waits `step_frames` (4-5) frames per step, which Styx still does when
  frames carry no lens report (`frame_exact = false` in the tuning, or the lens control did
  not report).
* **Contrast relative to level.** The windows' figure of merit over their squared green level:
  gradient energy then does not move with exposure and gain (AE settling during a scan,
  flicker), and the same code works on the PiSP's and the software ISP's units.
* **Noise-aware peak tests.** AF follows the contrast's frame-to-frame change while the lens
  stands still. A coarse scan stops at a drop only when the drop is more than three times that
  noise (far from focus the curve is flat and noisy and libcamera's scan stops there by
  chance), and a scan is reported focused only when the noise is below half of
  `1 − contrast_ratio` of the peak (in noise a flat curve passes libcamera's test by chance).
* **Failing gracefully.** A failed scan moves the lens to the range's default (hyperfocal)
  position, not the best point of a flat curve; until a scan succeeds again, continuous AF
  retriggers on colour or brightness changes only (contrast that is mostly noise would
  retrigger it forever).
* **Backlash.** The fine scan's samples are taken moving one way, so a lens with backlash
  sits on that side of each; the lens reaches the peak moving the same way (one fine step past
  it first when the peak lies behind the last sample).
* **Range ends.** A peak at the near or far end still gets three fine samples (libcamera's
  fine scan then runs off the end with two and no parabola).
* Contrast is the frame's own (libcamera's PDAF step runs before the frame's statistics are
  in and uses the previous frame's contrast). Switching to manual applies `lens_position` at
  once. Pausing continuous AF (libcamera's `AfPause`) is not implemented.

**Simulated** (`tests/sim_af.rs`, `sim::FocusSim`, run with `--nocapture`): Raspberry Pi's
IMX708 `rpi.af` tuning; the lens is the IMX708 module's map (0 D → 445, 15 D → 925) on a
10-bit VCM settling in 12 ms, with 4 codes of backlash and 0.5 codes of position noise; a
subject filling the middle of the image against a background at infinity, the figure of merit
halving 0.6 D from focus, 2% frame-to-frame error plus what the noise floor leaves of the
sensor noise (taken as before the gain), 30 fps with requests written in the frame the
statistics came from (the PiSP path).

| Case | Result |
|---|---|
| one-shot, subject at 2.5 D (40 cm) | focused 9 frames after the trigger, lens 2.513 D |
| one-shot, subject at 0.4 D (2.5 m) / 7 D (14 cm) | 8 / 14 frames, lens 0.388 / 6.982 D |
| one-shot, 2.5 D, frames without lens reports (libcamera's `step_frames` wait) | 48 frames, lens 2.482 D |
| continuous, subject at 1 D, then 4 D at frame 200 | focused at frame 12 (lens 0.98 D); refocused 20 frames after the change (4.007 D); no lens move while the scene is still, before or after |
| one-shot from 1 D to a subject at 3 D: CDAF / PDAF | lens within 0.1 D for good after 10 / 2 frames |
| continuous PDAF, subject moving 1 → 5 D over 2 s | 0.17 D behind at most during the move, 0.04 D after |
| 0.05 lux (AE at its limits, the curve is noise) | one-shot: failed, lens back at the default (0.92 D: backlash); continuous: one scan in 400 frames, failed |
| no texture (a blank wall) at 300 lux | failed, lens at the default (1.04 D); continuous: one scan in 400 frames |
| manual, 1 → 5 D | there within a few frames (1.5 D per frame, `max_slew`), state idle |

Hardware is the open question: the lens map, the settle time and the PDAF sign and gain are
Raspberry Pi's figures for the Camera Module 3 and have not been checked with Styx (TODO.md).

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
| `priors[] { lux, prior }`, `modes` (first = default), `bayes`, `min_G` (16-bit), `min_pixels`, `min_regions`, `coarse_step`, `whitepoint_r/b`, `bias_proportion`, `bias_ct`, `delta_limit`, `transverse_pos/neg`, `sensitivity_r/b`, `speed`, `frame_period`, `startup_frames` | same names; `min_g` ÷ 65536; `softness`, `hysteresis`, `hysteresis_mired` are Styx's own (0.2, 2, 25) |
| `rpi.alsc.calibrations_Cr/Cb`, `luminance_lut`, `corner_strength`, `asymmetry`, `luminance_strength`, `default_ct` | `alsc.calibrations_cr/cb`, …; `grid` from the table size (1024 → 32×32, 192 → 16×12) |
| `rpi.alsc.frame_period`, `startup_frames`, `speed`, `sigma`, `sigma_Cr`, `sigma_Cb`, `min_count`, `min_G` (16-bit), `omega`, `n_iter`, `threshold`, `lambda_bound` | same names in snake case (`sigma` sets both), `min_g` ÷ 65536; `n_iter` absent = width + height, 0 = calibration only |
| `rpi.ccm.ccms[] { ct, ccm }`, `saturation` | `ccm.ccms`, `ccm.saturation` |
| `rpi.contrast.gamma_curve` (16-bit x, y), `lo_*`, `hi_*`, `ce_enable` | `contrast.gamma_curve` ÷ 65535, `lo_max`/`hi_max` ÷ 65536 |
| `rpi.noise` | `denoise.noise.reference_constant/slope` |
| `rpi.denoise` (its `normal` mode, or the flat form) `.sdn/.cdn/.tdn`, `rpi.sdn` (VC4) | `denoise.sdn/cdn/tdn` (same keys; CDN `deviation` is the no-TDN one; without `tdn` SDN/CDN keep their no-TDN values) |
| `rpi.geq`, `rpi.dpc.strength`, `rpi.sharpen` | `denoise.geq`, `denoise.dpc`, `denoise.sharpen` |
| `rpi.af.ranges.{normal,macro,full}.{min,max,default}`, `speeds.{normal,fast}.*` (`step_coarse`, `step_fine`, `contrast_ratio`, `retrigger_ratio`, `retrigger_delay`, `pdaf_gain`, `pdaf_squelch`, `max_slew`, `pdaf_frames`, `dropout_frames`, `step_frames`), `conf_epsilon`, `conf_thresh`, `conf_clip`, `skip_frames`, `check_for_ir`, `map` | `af` with the same names (`macro` from `normal`, `full` from their union, `fast` from `normal`, as libcamera); `frame_exact` is Styx's own (true) |
| `rpi.hdr`, `rpi.cac`, `rpi.sync`, `rpi.nn.awb`, … | ignored (not implemented yet) |

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

`sim::FocusSim` (optional, `Simulation::focus`) adds a scene with depth and a focus lens
(see "AF: autofocus" above): focus statistics and phase data follow the lens's real position,
AF's `LensRequest`s move it with the settle time, backlash and noise of the model, and frames
carry the lens control's predicted report.

`sim::Simulation` models a scene (lux and colour temperature over frames, mains flicker with
any number of components and a start phase (`Simulation::set_time`), reflectances), a sensor (CT response, responsivity, shot and read noise, texture, line-quantised
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
| 100 Hz flicker ±7% on 8.1 ms frames at 120 fps (`sim_flicker.rs`) | | exposure × gain spread 5.4% and never locked without avoidance; none, locked at 7 with it |
| 0.05 lux (beyond AE's reach) at 30 fps, then 200 lux | | locked at its limits at frame 3, relocked 6 frames after the light came on |
| AWB 3000 K → 6000 K / 6000 K → 3000 K | 5994 K / 2999 K (6004 / 3007 before the soft search), gains within 0.1% of truth, settled (3%) in 54–64 frames at the default `speed` 0.05 |
| Late landings | none |

`replay`: JSON Lines, a header `{"styx_algo_replay": 1, "config": …}` then one
`{"stats", "meta", "params"?}` per frame. `Recorder` writes, `Recording::read` loads,
`replay::replay` reruns a pipeline and reports frames whose output differs from the recorded
one. Floats round-trip exactly.

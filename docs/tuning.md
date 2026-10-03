# Tuning a camera: `styx-tune`

`styx-tune` makes a tuning for a new sensor from raw captures of standard targets, the job
Raspberry Pi's Camera Tuning Tool (`ctt`) does for libcamera, in Rust and without libcamera.
It records the shots through Styx (`styx-tune capture`), calibrates from them
(`styx-tune calibrate`) and writes the tuning as Styx TOML and as a Raspberry Pi tuning file.

| Crate / tool | What |
|---|---|
| `crates/tune` (`styx-tune`) | Readers (Styx MCAP and raw recordings, DNG), chart detection, every calibration step, a synthetic sensor for validation |
| `tools/styx-tune` (binary `styx-tune`) | `capture`, `calibrate`, `preview`, `convert`, `synth` |
| `crates/algo` | The tuning model it fills (`Tuning`), the Raspberry Pi JSON import and export |

## What it calibrates

| Tuning section | From | Method |
|---|---|---|
| `black_level` (`r`, `g`, `b`, and `by_gain`) | dark shots | Per channel mean of covered frames, per analogue gain; or, with the lens uncovered, the level at zero exposure fitted from a ladder of exposures. Levels at several gains go into `by_gain` (Styx only; the black level algorithm interpolates by the frame's gain). Hot pixels are listed and turn on defective pixel correction |
| `alsc` (`calibrations_cr/cb` per colour temperature, `luminance_lut`, `sigma_cr/cb`) | flat fields at known colour temperatures | Cell means on the table grid (32×32 for the PiSP, `--grid 16x12` for VC4), `max(G)/G`, `G/R`, `G/B`, smoothed and normalised to a smallest gain of 1; the adaptive algorithm's sigmas from adjacent temperatures (as `ctt`) |
| `awb` (`ct_curve`, `transverse_pos/neg`; priors and modes when the base has none) | ColorChecker (its middle greys) or grey card at known temperatures | R/G and B/G through the lens shading tables; quadratic fit in `ctt`'s "hat space", margins from the points' distances, temperature kept monotonic |
| `ccm` (one matrix per temperature) | ColorChecker | Chart found automatically (or from four corners); patches lens-shading corrected, white balanced on the chart's greys; rows sum to 1; least squares in linear RGB, then Levenberg-Marquardt on the CIELAB error (ΔE) with the chart's exposure fitted; coefficients bounded (`--max-ccm`, 4) so the matrix cannot amplify noise without limit; clipped or black patches left out |
| `denoise.noise` (`reference_constant`, `reference_slope`) | bursts of a static scene (any shot of 2+ frames), else the chart's patches | σ = constant + slope·√level on the 16-bit scale at gain 1 (the Raspberry Pi model; samples at gain g divided by √g). Temporal: each pixel's variance across the burst after scaling out frame-to-frame brightness (lamp flicker), binned by level, the median per bin. Spatial fallback: the variance inside each patch after removing a plane |
| `lux` | the ColorChecker shot with a `lux` value | Mean Rec.601 luma of the white balanced frame (black removed, lens shading not corrected, as the statistics see it), its exposure and gain |
| `denoise.geq` | flat fields and patches | Gr−Gb difference against level: least-squares slope, offset raised so 99% of points lie under the line |

Not calibrated (taste, not measurement; kept from `--base` or the defaults): AGC metering,
targets and exposure profiles, the tone curve, denoise and sharpening strengths, AWB priors
(written as Raspberry Pi's usual shape when the base has none), CAC, HDR, autofocus.

## What a calibration session needs

### Equipment

| Item | What exactly | Why |
|---|---|---|
| ColorChecker | X-Rite / Calibrite ColorChecker Classic 24 (≈ 28 × 20 cm); a ColorChecker Passport works from closer | Colour matrices, AWB curve, noise, lux |
| Lights of known colour temperature | At least 3, better 5, spanning 2700-6500 K: a halogen or incandescent lamp (≈ 2800 K), high-CRI (Ra ≥ 95, R9 > 50) LED lamps at 3000, 4000, 5000 and 6500 K, or one tunable high-CRI LED panel; or a light booth (A ≈ 2856 K, TL84 ≈ 4000 K, D50, D65). Flicker-free (DC or high-frequency drivers) | Each temperature is one point of the AWB curve, one colour matrix and one lens shading table. Low-CRI LEDs have spiky spectra: their "colour temperature" does not describe what the sensor sees |
| Colour meter or spectrometer | Anything that reads CCT to ±50 K (Sekonic C-7000/C-800, a calibrated USB spectrometer) | Lamp labels are ±200 K or worse; the temperatures you enter are the calibration |
| Lux meter | Any calibrated one, ±5% | The lux reference (AE's lux estimate, the AWB priors' lux) |
| Diffuser | 3 mm opal acrylic or opal glass larger than the lens, or 3-4 sheets of plain white paper, held flat against the lens; alternatively a matte white wall or board, evenly lit, filling the frame out of focus | Flat fields for lens shading |
| Lens cap or black cloth | Fully opaque | Dark frames |
| Grey card (optional) | 18% neutral card | Extra AWB curve points |
| Mount | Tripod or bracket for the camera, a stand for the chart | Static scenes: the noise profile and the averaging need still frames |
| Room | Dark, no daylight or other lamps mixing in, matte black or grey surroundings | One light at a time, no coloured reflections |

### Setup

* Chart square to the camera, centred, filling a third to half of the frame, in focus.
  Light from one side at about 45° (or two matched lamps at ±45°) so there is no glare on
  the patches; even within ±5% across the chart. Measure the colour temperature and the lux
  at the chart, facing the light.
* Flats: diffuser against the lens, lit by the same lamp from in front; or the camera aimed at
  an evenly lit white wall with the lens defocused. The flat must be flat: shading of the
  light itself ends up in the luminance table.
* Same sensor mode as in use (the full array: the tables are resampled for crops), fixed focus
  as in use.

### Shots

| Shot | How many | Settings |
|---|---|---|
| Dark | one recording: 8 frames at each analogue gain used (1, 2, 4, 8, maximum) | lens covered, longest exposure of the mode |
| Flat field | one per light (≥ 2 temperatures, 3 recommended: warm, neutral, cool), 8 frames | gain 1; brightest channel at 70-80% of full scale on axis (under warm light the red channel is the brightest) |
| ColorChecker | one per light (≥ 3, ideally 5 across 2700-6500 K), 8 frames; one of them with a lux reading | gain 1; white patch's brightest channel at 70-80%, nothing clipped, black patch above the noise |
| Noise (optional) | 16 frames of a static scene at each gain | any scene with a range of levels; the chart and flat bursts count already |

About an hour with five lights. `styx-tune capture` walks through it and meters each bright
shot itself.

### Exposure rules

* Never clip: the brightest channel ≤ 80%; check the report's clipping warnings.
* Gain 1 for flats and charts (least noise); darks at every gain the camera uses.
* Steady light: flicker-free lamps, or exposures of whole mains half-periods (10 ms multiples
  at 50 Hz, 8.33 ms at 60 Hz). The noise profile scales out global flicker, the averages do not.
* AE and AWB off: raw frames at fixed exposure and gain (`capture` does this; for DNGs from
  other tools, fix them by hand).

## Commands

```sh
# Record the session through Styx (on the device; feature `capture`):
styx-tune capture --out session/                      # guided: dark, flats, charts, noise
styx-tune capture --out session/ --step flat --ct 4000 --yes
styx-tune capture --out session/ --step macbeth --ct 5000 --lux 800 --yes
styx-tune capture --out session/ --step dark --gains 1,2,4,8,15.5 --yes
styx-tune capture --out session/ --step black-series --gains 1,2,4,8 --yes  # no lens cover
styx-tune capture --out session/ --step noise --gain 4 --frames 16 --yes

# Find the chart (prints the corner patch centres for a session file; writes a picture):
styx-tune preview session/5000k_800l.mcap

# Calibrate (a directory with session.toml, ctt-style file names, or a session file):
styx-tune calibrate session/ --out tuned/ --name ov9782 [--base current.json] [--grid 16x12]

# Convert between Styx TOML and Raspberry Pi JSON:
styx-tune convert tuned/ov9782.toml ov9782.json

# Try it without a camera: a synthetic sensor's session
styx-tune synth /tmp/synthetic && styx-tune calibrate /tmp/synthetic --out /tmp/out
```

Build: `cargo build --release -p styx-tune-cli` (MCAP reading is on by default;
`--features capture` adds `capture`, which needs Styx's native stack). `capture` writes MCAP
recordings (raw frames with each frame's exposure and gains) named as `ctt` names its inputs
and appends each shot to `session.toml`.

### Inputs

* Styx MCAP recordings (`StreamRecorder`, format 2: per-frame exposure and gains).
* Styx raw recordings (`styx-pipeline::rawrec`, `.jsonl` + `.raw`, e.g. `native-pipeline --record`).
* DNG: uncompressed CFA DNGs (8/16 bit or packed 10/12/14; `ExposureTime`, `ISOSpeedRatings`
  as gain × 100, `BlackLevel`, `WhiteLevel`), through a small reader behind the
  `RawDecoder` trait (`Loader::with_dng` takes another decoder, e.g. `styx-dng`'s once merged).

A session file says what each capture is:

```toml
sensor = "ov9782"
[[shot]]
kind = "dark"            # dark | flat | macbeth | grey | noise
file = "dark.mcap"
[[shot]]
kind = "macbeth"
file = "5000k.mcap"
ct = 5000                # needed for flat, macbeth, grey
lux = 800                # optional: the lux reference
corners = [[212, 140], [1046, 152], [1040, 702], [205, 690]]   # optional
skip = 1                 # frames dropped at the start (default 1 with 3+ frames)
frames = 8               # at most this many
exposure_us = 20000      # for files that do not record them
gain = 2.0
```

`corners`: centres of dark skin, bluish green, black and white (clockwise from top-left as
printed), full-resolution pixels, when automatic detection fails (`preview` prints them when
it succeeds). Without a session file, a directory is read by `ctt`'s names: `dark*`/`black*`,
`alsc_<T>k*`/`flat_<T>k*`, `<T>k_<L>l*` or `<T>k*` (charts), `grey_<T>k*`, `noise*`.

Per shot, frames with the most common exposure and gain are used (dark shots keep all).

### Outputs

* `<name>.toml`: Styx's tuning (styx-algo's TOML), every section: measured ones replaced,
  the rest from `--base` (default: Styx's generic tuning, plus default AGC and tone curve so
  libcamera gets complete files).
* `<name>.json`: the same as a Raspberry Pi tuning file, version 2 (`Tuning::to_rpi_json_string`;
  target `pisp`, or `bcm2835` for 16×12 tables or `--target`). Styx-only settings (black levels
  by gain, AWB softness and hysteresis, AGC `full_step`) have no place there and are left out.
  Every libcamera pisp/vc4 tuning round-trips through import and export unchanged.
* `<name>-report.txt`: what was measured, how well (ΔE per matrix, fit errors, chart corners),
  and what could not be calibrated and why.

## How Styx picks the tuning up

A sensor description names its tuning (`tuning = "ov9782.json"`); `styx-pipeline::tuning`
looks for it in order:

1. `STYX_TUNING=<file>`: one file for every sensor (TOML or JSON by extension);
2. `STYX_TUNING_PATH` (colon separated), `~/.config/styx/tuning`, `/etc/styx/tuning`,
   `/usr/local/share/styx/tuning`, `/usr/share/styx/tuning`; in these, `ov9782.toml` is
   tried before `ov9782.json`, so `styx-tune`'s TOML (with black levels by gain) wins;
3. the tunings built into Styx (the OV9782's);
4. libcamera's Raspberry Pi directories (`/usr/{local/,}share/libcamera/ipa/rpi/pisp`);
5. Styx's generic tuning.

So: `install -D tuned/ov9782.toml /etc/styx/tuning/ov9782.toml`. For libcamera, the JSON goes
to `/usr/share/libcamera/ipa/rpi/pisp/<sensor>.json` (or `LIBCAMERA_RPI_TUNING_FILE`).

## Compared with `ctt`

| | `ctt` (Raspberry Pi, Python) | `styx-tune` |
|---|---|---|
| Inputs | DNG files named by temperature and lux | Styx MCAP and raw recordings (bursts with per-frame exposure and gain), DNG, `ctt` names or a session file |
| Capture | separate (`rpicam-still --raw`) | `styx-tune capture`: guided, metered, fixed settings, through Styx |
| Black level | from the DNG | measured per channel and per gain (covered, or extrapolated to zero exposure); hot pixels |
| Chart detection | template matching (OpenCV), corners not settable | uniform-region lattice and homography, orientation from the colours; manual corners |
| Lens shading | as here (median-blurred tables) | the same tables, [1 2 1] smoothed before normalising |
| AWB curve | greys through the colour tables, hat-space quadratic | the same |
| CCM | rows summing to 1, mean ΔE by Nelder-Mead, exposure from the mean level, colour tables only | rows summing to 1, sum of squared ΔE by Levenberg-Marquardt with the exposure fitted, full shading correction (luminance too), clipped patches out, coefficient bound |
| Noise | spatial, within the chart's patches (includes the print's texture and pixel non-uniformity) | temporal across a burst (robust median per level), the spatial estimate as fallback |
| Output | Raspberry Pi JSON | Styx TOML (+ black by gain) and Raspberry Pi JSON |
| Validation | none built in | a synthetic sensor with known response, shading and noise (below) |

## Validation

### Synthetic sensor (`crates/tune/tests/synthetic.rs`)

`styx_tune::synth::SensorModel`: 640×400 BGGR, 10 bit, black 64-64.6 codes per channel rising
0.3 codes per unit of gain, 2.5 e⁻/code, read noise 0.7 codes, two hot pixels, colour lens
shading that changes with temperature (corner fall-off to 0.46), a ColorChecker turned 4° with
some perspective on a graded background, 4 frames per shot, dark at gains 1/2/4, flats at
2800/4000/6500 K, charts at 2800/4000/5000/6500 K. Two colour responses: an exact one (known
CT curve and matrices) and a spectral one (Gaussian channel sensitivities, black-body light,
reflectance spectra fitted to the chart), where no exact matrix exists and the reference is the
fit on noise-free, unshaded patch values.

| Parameter | Recovered against true |
|---|---|
| Black level, 3 gains × 4 channels | within 0.02 codes |
| Hot pixels | both found, nothing else |
| Lens shading tables | shape within 0.09-0.14% (mean) and 0.4-0.9% (worst cell, a dim blue corner); scale within 0.5% |
| Chart | found in every shot, all 24 patches, corners within 1.5 pixels |
| AWB points | within 0.2% of noise-free patches through the same tables (exact and spectral); within 1% of the light's own R/G, B/G (the chart's greys are not perfectly neutral; 2% at 5000 K, where the tables are interpolated) |
| Colour matrices (exact) | every coefficient within 0.011 of the true matrix; ΔE76 0.53-0.60 (0.52 noise-free) |
| Colour matrices (spectral) | within 0.002-0.0075 of the noise-free fit; ΔE76 1.00-1.08 (0.95-1.04 noise-free; 15-16 without a matrix) |
| Noise profile | within 0.5% of the true σ from 1000 to 45000 (16-bit scale) |
| Lux | a shot at a quarter of the light and twice the gain estimated at 100.3 lux for 100 |
| Manual corners (2 px off) | the same matrix within 0.005 |

### The real OV9782 (CM5)

No chart, flat field or lights of known temperature were available: on 2026-10-03 (device
clock Aug 18) `styx-tune capture` recorded a black series (exposures 9.1 µs to 2 ms at gains
1, 2, 4, 8, 15.5, lens uncovered, dim room) and 16-frame bursts of the static room at each gain
(`target/device-tuning`, `crates/tune/tests/real_ov9782.rs` when present). Against the built-in
`ov9782.json` (made with libcamera's `ctt`):

| | `ov9782.json` | `styx-tune` | Notes |
|---|---|---|---|
| Black level (10-bit codes) | 64 all channels | R/Gr 65.5, Gb/B 64.9 at 1×; 64.4-64.7 at 2×; 64.7/65.3 at 4×; 65.5/66.5 at 8×; 66.1-66.5/67.8 at 15.5× (red rows / blue rows) | Extrapolated to zero exposure, not covered: the 1× levels and the row pattern agree with the 2026-10-02 check (pipeline.md: 64.74/65.34 by row at 1×, blue rows higher by 0.8-1.5 codes at 8-15.5×); at 8-15.5× today's levels sit 1.4-2.5 codes higher than then (the 1-line frames themselves read higher, so it is the sensor, e.g. its temperature, not the fit). Covered darks are the better input |
| Noise (16-bit, gain 1) | slope 5.38, constant 0 | slope 2.75, constant 38 (all gains, RMS error 4.5%); gain 1 alone: 3.01 + 14.6 | The temporal noise follows √gain as the model assumes (σ/√level 3.1, 4.4, 8-9.5, 10-13 at gains 1, 2, 8, 15.5). An earlier session's recording (20 ms × 2) gives 2.92. `ctt` measures inside the chart's patches, where the print's texture and pixel non-uniformity add to the noise: the file's profile is about 1.7× the measured temporal noise, and the denoise strengths in it were tuned against that |
| Lux reference | 800 lux at 21965 µs, Y 11460 | — | needs a chart shot with a lux reading |
| AWB curve | 2860-7580 K, 6 points | — | needs greys under lights of known temperature |
| Lens shading | 3000 and 5000 K tables | — | needs flat fields |
| Colour matrix | identity at 4000 K | — | needs the chart; the identity is the largest gap in the current tuning (≈ 15-20 ΔE76 on the synthetic sensors without a matrix) |
| GEQ | offset 239, slope 0.00766 | — | needs flats or a chart |

A real session needs the equipment above; `styx-tune capture` on the CM5 records it (checked on
the device: the black series and metered bursts at fixed gain; the other steps use the same
metering and recording).

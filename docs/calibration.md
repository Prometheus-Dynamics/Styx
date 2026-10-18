# Sensor calibration: `styx-calibrate`

`styx-calibrate` measures a sensor's black level, noise and defect pixels from raw dark frames,
and writes them as a **tuning overlay**: a JSON file Styx merges over the sensor's builtin tuning
at start-up. It is a library (`tools/styx-calibrate`, crate `styx-calibrate`, for other tools such
as a downstream app's action) and a command line (`styx-calibrate`). The camera tuning itself (colour, AWB,
AGC, CCM) stays with [`styx-tune`](tuning.md); the overlay carries only what a dark frame can tell.

## What the dark sweep measures

For each capture (one exposure and one analogue gain), per CFA channel R, Gr, Gb and B:

| quantity | how |
|---|---|
| black level | the median of the per-pixel temporal means, defect pixels left out (robust); the mean of every sample is reported beside it |
| read noise | the temporal standard deviation of each pixel, averaged over the channel |
| DSNU | the spatial standard deviation of the per-pixel temporal means |
| row / column FPN | the spread of each row's and each column's temporal mean |
| row / column temporal noise | the temporal standard deviation of each row's and column's mean, averaged |
| defect pixels | hot: the temporal mean is more than `k` (default 6) times the channel's read noise above black; stuck: no temporal variation; noisy: far more variation than the channel's |
| clipped black | the fraction of samples that are exactly 0 |

Across captures:

- the black level per gain (per channel), and whether it moves with gain (more than 0.5 DN across
  gains, `--black-move`);
- the read noise per gain, fitted as `constant + slope × gain` (the fit is reported; the overlay
  uses the per-gain values, see below);
- the dark current per gain: the slope of the mean signal against exposure time (DN per second).

## The dark flow

1. **Capture dark frames** on the board, with no light reaching the sensor (lens capped or the
   camera face-down, every board LED off; on the CM5 dev board the activity LED is active-low, so
   `brightness=1` turns it off). Take 60 frames after about 40 settling frames, at every
   combination of analogue gain and exposure you want the overlay to cover. A sensible grid for
   the OV9782 is gains 1, 2, 4, 8 by exposures 8 ms and 30 ms. Each setting goes in its own
   directory, named `e<exposure_us>_g<gain>/`.

   The capture tool is outside this repository. Its output for each directory is **one raw dump in
   this format**:

   - `frames_<W>x<H>_bggr16.raw`: 16-bit little-endian samples, the 10-bit values in the low bits,
     frames back to back, rows top to bottom, no padding. The CFA order and size can also come
     from the arguments (`--cfa`, `--size`) when `meta.txt` lacks them;
   - `frames.csv`: one row per frame, with a header naming at least `exposure_us` and
     `analog_gain`. A `temperature` column is used for the temperature range when present;
   - `meta.txt`: `key=value` lines: `width`, `height`, `cfa` (for example `BGGR`), `bit_depth`
     (default 10), and optionally `sensor` (or `camera=ov9782 ...`) and `unit`.

   The exposure and gain the frames were actually taken at matter more than the requested ones:
   the analysis uses the csv's, and reports whether they held over every frame.

2. **Analyse** the directories:

   ```sh
   styx-calibrate dark e8000_g1.0 e8000_g2.0 e8000_g4.0 e8000_g8.0 \
                       e30000_g1.0 e30000_g2.0 e30000_g4.0 e30000_g8.0 \
       --out calibration/ov9782 --sensor ov9782 --unit <serial>
   ```

   The options:

   - `--out DIR` (default `styx-calibration`): where the outputs go;
   - `--sensor NAME`: the tuning's name stem (`ov9782` for `ov9782.json`); from the captures when
     they name it;
   - `--unit ID`: the unit's serial or name, written into the overlay;
   - `--cfa BGGR`, `--size WxH`, `--bits N`: overrides for what the capture files do not say;
   - `--hot-k K` (default 6), `--black-move LSB` (default 0.5): the analysis thresholds;
   - `--per-channel-black`: write each channel's black level, Gr and Gb apart, instead of the
     mean of the four (see "The board finding");
   - `--noise-model`: write the `rpi.noise` block (the read-noise constants; off by default);
   - `--styx-commit HASH`, `--date YYYY-MM-DD`: provenance (the commit defaults to the checkout's
     `HEAD`, the date to today).

3. **Check** the processed output. Dark frames through Styx's own processing (the PiSP ISP, the
   builtin tuning, TDN on) are a grey `frames_<W>x<H>_gray.raw` per directory. Before and after
   the overlay:

   ```sh
   styx-calibrate check-processed processed/e8000_g1.0 processed/e8000_g8.0
   ```

   This reports the fraction of pixels that are exactly 0, the mean, and the temporal noise (the
   mean and the 99th percentile of the per-pixel standard deviation). A black level that is right
   at every gain keeps the clipped fraction near zero. The ov9782 without an overlay clips 40 to
   55 % of its dark pixels above 1× gain: that is the bug the overlay is meant to fix, not a
   property of the sensor.

4. **Install** the overlay (below) and capture again.

## The outputs

`styx-calibrate dark` writes two files into `--out`:

- `calibration.md`: the report. The captures table, the black level and read noise per gain,
  the read-noise fit, the dark current, the defect pixels (the first 200; the overlay has all),
  and notes on the mapping below.
- `<sensor>.overlay.json`: the tuning overlay.

The report and the overlay are both written from the same sweep. The overlay is checked before it
is written: it must parse in Styx (merged over a Raspberry Pi tuning with a noise block), or the
command fails.

### The overlay format

By default the overlay carries one black level per gain (the mean of the four channels) and the
provenance and measurements. Shortened, with the numbers of the OV9782 run of 2026-10-13:

```json
{
  "overlay_version": 1,
  "sensor": "ov9782",
  "unit": "my-cm5",
  "date": "2026-10-13",
  "styx_commit": "43a9517cbd7e",
  "temperature_c": { "min": 47.95, "max": 54.55 },
  "source": "styx-calibrate dark: 18 captures, 6 gains, 10-bit raw",
  "units": "black levels and noise constants on the 16-bit scale (raw x 2^(16 - bits)); exposure in microseconds",
  "algorithms": [
    { "rpi.black_level": {
        "black_level": 4172.8,
        "by_gain": [
          { "gain": 1.0, "black_level": 4172.8 },
          { "gain": 2.0, "black_level": 4106.45 },
          { "gain": 4.0, "black_level": 4114.99 },
          { "gain": 7.75, "black_level": 4117.76 },
          { "gain": 10.0, "black_level": 4116.06 },
          { "gain": 15.88, "black_level": 4096.43 }
        ] } }
  ],
  "defect_pixels": [[12, 14], [640, 401]],
  "measured": {
    "gains": [ { "gain": 1.0, "captures": 3, "black_raw_dn": [65.5, 65.5, 64.9, 64.9],
                 "read_noise_raw_dn": [0.66, 0.66, 0.66, 0.66] } ],
    "dark_current": [ { "gain": 1.0, "exposures": 3, "dn_per_s": 12.5 } ],
    "read_noise_fit_raw_dn": { "constant": 0.2, "slope_per_gain": 0.3 }
  }
}
```

(The black levels are from the 2026-10-13 run. The `measured` block is abridged, and its numbers
are illustrative.)

- `algorithms` holds the Raspberry Pi tuning's blocks, so a merge into a libcamera-style tuning
  means what it means there. Black levels are on the **16-bit scale** (the raw value times
  `2^(16 - bits)`, so 64 at 10 bits is 4096), as the tuning files use.
- `rpi.black_level`: `black_level` is the level, the mean of the four channels. `by_gain` lists it
  per gain and is written only when the level moves with gain by more than `--black-move` (0.5 raw
  DN by default). Otherwise the top-level level is the mean over gains.
- `defect_pixels`: sensor coordinates `[x, y]` of the hot, stuck and noisy pixels found in any
  capture. Styx reads it but does not yet apply it (the back end's defective pixel correction has
  no per-pixel table); it is kept for the tools.
- `measured`, `temperature_c`, `unit`, `date`, `styx_commit`, `source`: provenance and the raw
  numbers. Styx ignores them. `measured` keeps the per-channel black levels and the read noise
  per gain whether or not they are written to the blocks above.

Two blocks are written only on request:

- `--per-channel-black`: each channel's level in the `rpi.black_level` block, with the greens
  apart: `black_level_r`, `black_level_gr`, `black_level_gb`, `black_level_b` (Styx's
  `black_level_gr` and `black_level_gb` are not in libcamera's files), and `black_level_g` (their
  mean), at the top level and in each `by_gain` entry. See "The board finding" below before using
  it: it is not the default for the OV9782.
- `--noise-model`: an `rpi.noise` block with `reference_constant` (the read noise at unity gain's
  scale, see the mapping below) and, where there are several gains, a `by_gain` list of constants.
  The overlay never sets `reference_slope`, which the tuning keeps. It is off by default: see
  "The board finding".

Overlay blocks deep-merge over the tuning's blocks of the same name (keys merge key by key;
lists such as `by_gain` are replaced whole). A block the tuning lacks is added.

### The noise mapping

Written only with `--noise-model` (see "The board finding").

The denoise code models the noise of a pixel of level `L` (on the 16-bit scale) as
`constant + slope × √L`, and scales both by `√gain` for each frame
(`crates/algo/src/algos/denoise.rs`). Dark frames sit at the black level, which is `L = 0` once
the black level is subtracted (the assumption this mapping rests on), so the dark read noise is
the constant alone, and:

- measured read noise at gain `G`: `σ(G)` (raw DN, the mean over the channels), in the 16-bit
  scale `σ(G) × 2^(16 - bits)`;
- the constant at unity gain's scale: `reference_constant = σ(G) × 2^(16 - bits) / √G`, one per
  gain (`by_gain`); the denoise code multiplies it by the frame's `√gain` and gets `σ(G)` back at
  gain `G`.

The slope is not measured by dark frames (it needs flat fields, the photon transfer curve), so the
tuning's `reference_slope` is kept.

## Installing the overlay

Styx loads an overlay for a sensor from, first match:

1. the configuration: `StyxConfig::new().tuning_overlay("/path/ov9782.overlay.json")`;
2. the environment: `STYX_TUNING_OVERLAY=/path/ov9782.overlay.json`;
3. `<sensor>.overlay.json` in a tuning directory: `/etc/styx/tuning/ov9782.overlay.json` (or
   `STYX_TUNING_PATH`, `~/.config/styx/tuning`, the other system directories);
4. the built-in overlay for the sensor, when built-in tunings are on. The OV9782 has one
   (`crates/pipeline/tuning/ov9782.overlay.json`): the mean black level per gain from the
   2026-10-13 dark sweep below, without the defect list. Any of 1–3 replaces it.

```sh
sudo install -D calibration/ov9782/ov9782.overlay.json /etc/styx/tuning/ov9782.overlay.json
```

An overlay is used only when:

- the tuning it merges over is a Raspberry Pi JSON tuning (the builtin `ov9782.json`, or a
  `.json` file in the search path). A tuning in Styx's TOML is not overlaid (`styx-tune`'s
  `ov9782.toml` wins over the JSON, and its own black levels apply);
- its `sensor` key, if it has one, names the tuning's sensor (the file stem). An overlay for
  another sensor is skipped.

When a capture starts, the log names the tuning and the overlay: `tuning=builtin:ov9782.json
overlay=/etc/styx/tuning/ov9782.overlay.json` (or `overlay=builtin:ov9782.overlay.json`). If an overlay is found but does not parse, the log
says `overlay-rejected=<path>: <reason>` and the tuning is used without it.

With no overlay installed and no built-in one for the sensor, nothing changes: the tuning, and so
every trace, is the same as before.

## What the ISP does with the levels

- **Per-channel black level.** The PiSP back end's black level correction can subtract one level
  per CFA channel (R, Gr, Gb, B), written with `--per-channel-black`. The SDN and TDN denoise
  blocks, which take one level, use the mean of the four. For the OV9782 the per-channel level is
  not the default (see "The board finding"): the overlay's default is the mean of the four.
- **Front end and statistics.** The front end keeps the lowest of the four levels as its pedestal,
  as before, so the raw output and the statistics are unchanged for a uniform tuning.
- **Software ISP.** Uses one level (the lowest); a per-channel level is a PiSP feature for now.
- **Per gain.** A `by_gain` list is interpolated linearly in gain between its entries, the nearest
  one outside them. Noise constants per gain work the same way.

## The board finding (OV9782, 2026-10-13)

A board A/B on the processed Y at 8 ms, gains 1, 7.75 and 15.9, compared the overlay's two
options:

- **Per-channel black level over-corrects on the processed path.** With the greens apart, the
  output carried an even/odd row stripe of +0.31, −0.36 and −0.93 8-bit levels at those gains.
  Swapping Gr with Gb and R with B flipped the sign, so the stripe follows the Bayer phase. The mean
  of the four channels per gain leaves about 0.00 at every gain. The raw row-parity offset in the
  dump does not reach the back end the same way, so the per-channel correction is wrong on this
  path. Both options lift the 1× floor by the same amount (mean 2.61 → 0.41, zeros 0 % → 70 %).
- **The dark-frame noise constants make the denoise much stronger.** With the `rpi.noise` block
  (44 to 89 on the 16-bit scale, where the builtin tuning has 0), the processed temporal noise fell
  by 4 to 6 times. That is a denoise retune, not a calibration fix. It needs a lit-scene check for
  detail before any use.

So the overlay's defaults are the mean of the four channels per gain and no `rpi.noise` block. The
per-channel black level stays in the PiSP back end and the pipeline, tested, for other sensors or a
later fix; it is not the default for the OV9782. The measurements are in the overlay's `measured`
block either way.

A lit-scene check after the back-end tiling fix (8 ms, 7.75×, the AprilTag scene, edge energy of
100-frame means against libcamera's) decided the defaults:

| Overlay | Tags | Blinds | Whole | Temporal noise | AprilTag C |
|---|---|---|---|---|---|
| none | 100 % | 101 % | 110 % | 0.90 | tags 1, 3, 5 at 100 %, tag 6 36 % |
| mean black per gain | 101 % | 102 % | 111 % | 0.85 | tags 1, 3, 5 at 100 %, tag 6 45 % |
| black + noise model | 98 % | 95 % | 100 % | 0.57 | — |

(Tag 6 is distant and oblique and sits at the decision-margin cut in every set, libcamera's
included: 47 %.) The black level per gain costs no detail, so it is the OV9782's built-in overlay.
The noise model loses 5 % on fine detail (the blinds), short of the 98 % bar, so it stays opt-in
(`--noise-model`).

## Limits

- Dark frames give the black level, the read noise and the dark current. Shot noise (the slope of
  the noise model) and the lens shading need flat fields, which this tool does not capture yet.
- The defect list is recorded, not applied. The back end's DPC has strength settings, not a list.
- The dark current is reported, not applied. Styx's noise model has no dark-current term; the
  OV9782 dark run of 2026-10-12 showed no change between 8 ms and 30 ms, so there was nothing to
  model yet.
- The overlay describes one sensor in one thermal state: the temperature range is in the overlay so
  a later sweep at another temperature can be compared.

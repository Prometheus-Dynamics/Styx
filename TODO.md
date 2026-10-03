# Styx native stack: progress and TODO

Styx as a stand-alone camera stack: USB and CSI cameras with nothing but the Linux kernel
underneath. No libcamera, no `v4l` crate, no per-sensor kernel driver. Design and device rules:
[docs/native-stack/README.md](docs/native-stack/README.md). Numbers below are from the CM5 dev
box (OV9782 1280x800) unless stated.

## Done

### Foundations and stand-alone (phases 0–1)
- [x] Pure-Rust kernel interfaces (`styx-kernel`): V4L2, media controller, subdevs, events,
      dma-heaps, I²C, GPIO.
- [x] V4L2 backend on `styx-kernel`; the `v4l` crate is gone everywhere.
- [x] libcamera fully optional: no default build pulls it, CI job without it, checks in
      `scripts/check-feature-combinations.sh`.

### Sensors (phase 2)
- [x] Sensors as data (`styx-sensor`, TOML descriptions) driven from Rust through one generic
      kernel module, `styx-sensor-bridge`.
- [x] Sensors with an upstream kernel driver run natively too (subdev controls, DelayedControls-style
      scheduling); data files for ov9281/ov9782 (verified), imx219, imx477, imx708, ov5647
      (unverified). See [adding-a-camera.md](docs/native-stack/adding-a-camera.md).
- [x] Frame-exact typed controls; exposure/gain land on the predicted frame, verified by embedded data.
- [x] Exact frame rates (VTS + 1 lines); 30/60/120 fps.
- [x] Burst I²C writes, 400 kHz cam0 bus, parallel bring-up: open → first frame 29 ms
      (libcamera 131–142 ms).
- [x] Robustness: restart with held frames, error paths end clean, supervised reconnect, kill -9
      recovery, rmmod refused while streaming, `down.sh` never unbinds rp1-cfe under a user
      (that was the cause of the unexplained kernel oopses/reboots), 2×15 min soak with no leaks.

### Pi ISP and algorithms (phases 3–4)
- [x] PiSP front end statistics + back end processing, dma-buf hand-off, both outputs, pyramid level.
- [x] 3A in Rust (`styx-algo`): AGC, AWB (Bayesian, made continuous + hysteresis), adaptive ALSC
      (ported, matches libcamera to 1e-12), CCM, contrast, denoise/sharpen from tuning, TDN.
- [x] AE locked at frame 6 (229 ms from open; libcamera 664–675 ms); warm restarts lock by frame 2.
- [x] PiSP path CPU 0.26 ms/frame through Styx (0.8 % of a core; libcamera 2.9 %).
- [x] Image quality matches libcamera within libcamera's own session-to-session spread.
- [x] Flicker avoidance (`Flicker::Auto`, the Styx default; `AE_FLICKER_MODE`): AE fits the
      lamp's flicker (harmonics of 50/60 Hz mains) from the frames and meters against the
      mean light, so 120 fps no longer chases it (exposure × gain spread 10% → 1%, cold start
      never locked → frame ~21 under the room's ±20% 50 Hz lamp); long exposures whole mains
      periods (30 fps: frame spread 2.4% → 0.2%); detects the mains frequency itself.
- [x] Deflicker (`Deflicker::Auto`, on with flicker avoidance; `AE_DEFLICKER_MODE`): each
      frame's ISP digital gain divided by the brightness the flicker model predicts for that
      frame (mains frequency tracked, confidence-gated, faded, headroom only where highlights
      would clip): output frame-to-frame spread under the room's lamp 15-18% → 3.4-4.7% at
      120 fps, 16-19% → 4.3-4.9% at 90, 3.2-3.6% → 1.5-1.6% at 60 (simulated 13% → 0.12%);
      PiSP CPU 0.30 → 0.48 ms/frame at 120 fps (algorithms on every frame while it flickers).
- [x] AE locks at its limits in scenes beyond its reach (`AE_STATE` converged, as libcamera).
- [x] Software ISP (`styx-softisp`): fp16 NEON path, cached capture read in place, 15 Hz stats
      when settled — 8.8–9.7 % of a core at 30 fps on one A76 core (was 47 %), bit-exact integer
      path elsewhere; x86 tone curve as fixed-point quadratics (`Arithmetic::IntPolyTone`, within
      a code; RGB24 frame 1.59 → 1.11 ms on Zen 3) and an exact AVX2 table.
- [x] Built-in OV9782 description and tuning; Styx tuning search path (`STYX_TUNING_PATH`, …).

### Ecosystem (phase 5)
- [x] GStreamer `styxsrc` + device provider (`crates/gst-styx`).
- [x] PipeWire camera node daemon (`crates/pipewire-styx`).
- [x] Frame socket with leases (`styx::ipc::FrameSocket`, HeliOS wire format).
- [x] Examples for every task (listing, planned capture, async, controls and metadata, shared
      captures, other processes, raw frames and recordings, hotplug, adding a camera), run on the
      CM5; [docs/comparison.md](docs/comparison.md) against libcamera, V4L2 and GStreamer, with a
      fresh side-by-side run (open → first frame 34 vs 101 ms, CPU 1.1 vs 2.7 %, PSS 25.6 vs
      30 MiB).
- [x] Processed captures take AE/AWB controls (AE on/off, fixed exposure or gain, EV, AWB
      on/off, colour temperature, red/blue gains) and report AWB's colour temperature.

### HeliOS
- [x] `helios-peripherals` runs on the native stack (HeliOS branch `styx-native-trial`):
      2 h with helios-engine consuming, 3.7 % CPU, 30 MB, no stalls; ship checklist in
      [helios-trial.md](docs/native-stack/helios-trial.md).
- [x] CM5 dev box boots the bridge overlay (`camera-mode.sh native|libcamera|status`).

## TODO

### Decisions
- [x] Default PiSP output buffer count: 6 (two frame-socket consumers holding 500 ms: 17 fps
      with 4, 30 with 6; +3.1 MB CMA for NV12 1280x800), plus the planner's extra buffers
      within half the free CMA (a slow shared consumer no longer slows the others).
- [ ] When HeliOS drops libcamera (checklist in helios-trial.md).

### HeliOS (branch `styx-native-trial`)
- [ ] Merge the branch; build helios-peripherals without the `libcamera` feature.
- [ ] Publish frames through `styx::ipc::FrameSocket` and keep the engine's connection open while
      it holds a frame (real leases).
- [ ] Image: drop libcamera/libpisp, add the bridge module (Buildroot snippet in
      `kernel-modules/styx-sensor-bridge/buildroot`), config.txt overlay lines.
- [ ] Not yet tested: a real CSI unplug, controls set by HeliOS, runs longer than 2 h.

### Image quality
- [x] Black level at high gain: not a black level difference. Zero-exposure levels through the
      bridge and through the `ov9282` driver agree within 0.1 code at 1-15.5× (BLC registers the
      same); the 2.4 codes were most likely the 50 Hz lamp. To close it against libcamera
      itself: its raw at a 1-line exposure and 8× (pipeline.md, "Quality vs libcamera").
- [ ] Verify the unverified kernel-sensor data files on real cameras (imx219, imx477, imx708, ov5647).
- [ ] OV9782 tuning of our own (today: the HeliOS tuning).

### Performance
- [x] PiSP: 0.12 ms/frame was the `pispbe` driver writing its whole config per job with a
      barrier per word. Patched driver (`kernel-modules/pispbe`: relaxed MMIO, only changed
      words, cached config copy): 117 → 8 µs per job, Styx API 0.35 → 0.23-0.26 ms/frame at
      30 fps (0.25 → 0.13-0.15 at 120), latency −0.11 ms, outputs bit-identical, libcamera
      unaffected. Installed on the dev box as an override of `pisp_be`.
- [ ] Ship the `pispbe` patch in the HeliOS image (kernel patch) and/or send it upstream (draft
      in `kernel-modules/pispbe/README.md`; ask Raspberry Pi whether the config registers are
      guaranteed to keep their values between jobs).
- [ ] Software ISP: outside the image maths only ~0.3 ms of memory traffic and ~0.15 ms of
      dequeue/sync/bookkeeping are left (staging copy and end-of-access sync gone). Integer path
      (Pi 4 class): colour matrix 1.1 ms and tone table 1.5 ms per frame on the A76; not yet
      measured on a real Cortex-A72. AVX-512 VBMI exact tone table (untested: no AVX-512 host).
- [ ] PipeWire node copies each frame once (zero-copy needs the camera buffers as the PipeWire pool).

### Platforms
- [ ] A second bridged sensor and a non-Pi board (software ISP or its own ISP).
- [ ] GPU ISP path where Vulkan exists (not on the HeliOS image).
- [ ] Userspace UVC (optional).

### Known issues
- [ ] rp1-cfe leaks one device-tree node per runtime overlay up/down (upstream; dev runtime path only).
- [x] 120 fps AE sometimes chases 100 Hz flicker (kernel-driver path); add anti-flicker.
- [x] Flicker stays in the frames at exposures shorter than a period: deflicker (above).
- [ ] Deflicker: rolling-shutter band gains unverified on a sensor (needs `readout` from the
      sensor description); 30 fps with short exposures leaves ~2% (50/100 Hz alias together).
- [ ] With deflicker off, a clipped lamp in view under flicker makes AE chase the beat at
      120 fps (simulated, also before deflicker; with deflicker on it locks).
- [x] `AE_STATE` never reported converged when AE's target was out of reach; AE now reports
      converged once pinned at its limits (`AeStatus::at_limit`), as libcamera does.
- [x] OV9782 raw8 (`BA81`) through the bridge: frames reported 342.9 ms × 0.5 at 30 fps and were
      far darker than RAW10. The raw8 embedded line carries each 10-bit word's top 8 bits
      (decoded as RAW10 it said VTS 4, and the schedule then limited exposure to that 4-line
      frame), and the raw8 PLL runs at 192 MHz, not the driver's 200 MHz (30 fps ran at 28.8).
      raw8 now has `pixel_rate = 192 MHz` and `embedded_data = false` (predicted values):
      30 / 120 / 5 fps exact, levels equal RAW10's top 8 bits at the same exposure and gain.
- [x] Shared captures on the PiSP: a slow consumer with a deep queue paced the camera for every
      consumer. Back end buffers now default to 6 plus the planner's `capture_extra_buffers`
      (within half the free CMA); a consumer sleeping 200 ms gets 5 fps, the other keeps 30.
- [x] MCAP recordings drop a native frame's exposure and gains (`NativeFrameMeta`): MCAP format
      2 and `.styxrec` 2 record exposure, gains, frame duration and length, verified, error;
      version 1 files still read. AE state, colour temperature and lux are controls, not
      per-frame metadata, so not recorded (add them to `NativeFrameMeta` first).
- [x] A plan that asks no frame rate gets the mode's fastest (camera service clients: 260 fps
      at 640x400): modes with a rate range now run at `planner::DEFAULT_FPS` (30, within the
      mode's range) without `min_fps`, in single and shared plans and plain native captures.
- [ ] A frame-length or exposure value decoded from embedded data is trusted even when it is
      impossible (VTS 4 on a 800-line mode); the control schedule could reject such reports.

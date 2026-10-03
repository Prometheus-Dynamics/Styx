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
- [x] AE locks at its limits in scenes beyond its reach (`AE_STATE` converged, as libcamera).
- [x] Software ISP (`styx-softisp`): fp16 NEON path, cached capture, 15 Hz stats when settled —
      9.4–10.2 % of a core at 30 fps on one A76 core (was 47 %), bit-exact integer path elsewhere.
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
- [ ] PiSP: 0.12 ms/frame is the `pispbe` driver rewriting its whole config per job (kernel side).
- [ ] Software ISP: remaining ~1 ms outside the ISP (row copies, dequeue); x86 tone curve.
- [ ] PipeWire node copies each frame once (zero-copy needs the camera buffers as the PipeWire pool).

### Platforms
- [ ] A second bridged sensor and a non-Pi board (software ISP or its own ISP).
- [ ] GPU ISP path where Vulkan exists (not on the HeliOS image).
- [ ] Userspace UVC (optional).

### Known issues
- [ ] rp1-cfe leaks one device-tree node per runtime overlay up/down (upstream; dev runtime path only).
- [x] 120 fps AE sometimes chases 100 Hz flicker (kernel-driver path); add anti-flicker.
- [ ] Flicker stays in the frames at exposures shorter than a period (AE no longer chases it):
      per-frame ISP digital gain from the flicker model could take it out.
- [ ] `AE_STATE` never reports converged when AE's target is out of reach (exposure and gain at
      their limits in a dark room); libcamera reports converged at the same final exposure
      (500 ms after open, docs/comparison.md).
- [ ] OV9782 raw8 (`BA81`) through the bridge: frames report 342.9 ms × 0.5 at 30 fps and are far
      darker than RAW10 at the same exposure and gain (embedded data decoded as RAW10? the raw8
      registers?). RAW10 and the processed modes are fine.
- [ ] Shared captures on the PiSP: frames queued for consumers hold back end buffers (4 per
      output) and the native path ignores the planner's `capture_extra_buffers`, so a slow
      consumer with a deep queue (`Priority::Power`: 3) paced the camera for both consumers
      (20 fps); the examples ask for queue depth 1.
- [ ] MCAP recordings drop a native frame's exposure and gains (`NativeFrameMeta`).
- [ ] A plan that asks no frame rate gets the mode's fastest (camera service clients: 260 fps
      at 640x400); consider a default rate for services.

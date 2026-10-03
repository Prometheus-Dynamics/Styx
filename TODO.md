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
- [x] Software ISP (`styx-softisp`): fp16 NEON path, cached capture read in place, 15 Hz stats
      when settled — 8.8–9.7 % of a core at 30 fps on one A76 core (was 47 %), bit-exact integer
      path elsewhere; x86 tone curve as fixed-point quadratics (`Arithmetic::IntPolyTone`, within
      a code; RGB24 frame 1.59 → 1.11 ms on Zen 3) and an exact AVX2 table.
- [x] Built-in OV9782 description and tuning; Styx tuning search path (`STYX_TUNING_PATH`, …).

### Ecosystem (phase 5)
- [x] GStreamer `styxsrc` + device provider (`crates/gst-styx`).
- [x] PipeWire camera node daemon (`crates/pipewire-styx`).
- [x] Frame socket with leases (`styx::ipc::FrameSocket`, HeliOS wire format).

### HeliOS
- [x] `helios-peripherals` runs on the native stack (HeliOS branch `styx-native-trial`):
      2 h with helios-engine consuming, 3.7 % CPU, 30 MB, no stalls; ship checklist in
      [helios-trial.md](docs/native-stack/helios-trial.md).
- [x] CM5 dev box boots the bridge overlay (`camera-mode.sh native|libcamera|status`).

## TODO

### Decisions
- [ ] Default PiSP output buffer count (`native_output_buffers`, now 4; 6–8 keeps 30 fps with slow
      frame holders, 1.5 MB each).
- [ ] When HeliOS drops libcamera (checklist in helios-trial.md).

### HeliOS (branch `styx-native-trial`)
- [ ] Merge the branch; build helios-peripherals without the `libcamera` feature.
- [ ] Publish frames through `styx::ipc::FrameSocket` and keep the engine's connection open while
      it holds a frame (real leases).
- [ ] Image: drop libcamera/libpisp, add the bridge module (Buildroot snippet in
      `kernel-modules/styx-sensor-bridge/buildroot`), config.txt overlay lines.
- [ ] Not yet tested: a real CSI unplug, controls set by HeliOS, runs longer than 2 h.

### Image quality
- [ ] Black level at high gain: raw ~2.4 codes (10-bit) lower than libcamera's at 8× gain; confirm
      with a covered lens and fix in the description.
- [ ] Verify the unverified kernel-sensor data files on real cameras (imx219, imx477, imx708, ov5647).
- [ ] OV9782 tuning of our own (today: the HeliOS tuning).

### Performance
- [ ] PiSP: 0.12 ms/frame is the `pispbe` driver rewriting its whole config per job (kernel side).
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
- [ ] 120 fps AE sometimes chases 100 Hz flicker (kernel-driver path); add anti-flicker.

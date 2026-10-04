# Styx native stack: progress and TODO

Styx as a stand-alone camera stack: USB and CSI cameras with nothing but the Linux kernel
underneath. No libcamera, no `v4l` crate, no per-sensor kernel driver. Design and device rules:
[docs/native-stack/README.md](docs/native-stack/README.md). Numbers below are from the CM5 dev
box (OV9782 1280x800) unless stated.

## Done

### Foundations and stand-alone (phases 0–1)
- [x] Pure-Rust kernel interfaces (`styx-kernel`): V4L2, media controller, subdevs, events,
      dma-heaps (I²C, GPIO and uevents moved to Lemnos 2.0, `lemnos-linux`).
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
- [x] Regions of interest from the PiSP for every consumer (`FrameRequest::regions`, up to 16
      each): the main output's crop, the second output's, extra back end passes over the same
      raw frame (0.03 ms + 1.35 ns/pixel of back end time, ~0.03 ms CPU per pass; 4 regions
      0.385 ms CPU and 9.1 ms latency per frame against 0.29 ms / 8.9 ms for one), regions
      paired with their frame (companions), temporal denoise read but not written by passes,
      `skip_stale_regions`; overview box-filtered without a PiSP. See
      [frame-planning.md](docs/frame-planning.md#region-of-interest).
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
- [x] Regions of interest without a PiSP: the software ISP processes only the region (bit-exact
      against the whole frame's crop) and bins the overview, statistics still of the whole frame
      (CM5 1280x800: 3.2 → 1.3 ms CPU per frame for a 320x200 region + overview, AE/AWB
      unchanged); libcamera ROI plans crop per output with `rpi::ScalerCrops` (2-3 frames late).
- [ ] Regions, remaining: several regions per client in the planner and the camera service
      (the software ISP already takes a list, `SoftParts::regions`); a true 4x4 bin for the
      software overview beyond 2 (today every second quad of every second quad row); libcamera
      regions of a size other than the first are scaled to it (reconfigure, or a second crop
      stream); the GPU ISP and binned software modes do not crop.

### Autofocus (simulation only so far)
- [x] AF in Rust (`styx-algo`, from Raspberry Pi's `af.cpp`): PDAF loop, coarse + fine contrast
      scans with parabola fits, continuous mode with scene-change retriggering, windows with
      weights, manual / auto / continuous, `rpi.af` tuning import; Styx changes: frame-exact
      scan steps on lens reports, contrast relative to level, noise-aware peak tests, failed
      scans back to hyperfocal, backlash-aware approach. Simulated (`tests/sim_af.rs`): one-shot
      9 frames (48 with libcamera's frame counting), within 0.02 D; continuous refocus after a
      depth step in 20 frames with no hunting; PDAF 2 frames against 10; low light and blank
      walls fail cleanly (algorithms.md, "AF: autofocus").
- [x] Lenses as data (`styx-sensor::lens`): kernel lens drivers found by the sensor's ancillary
      link (`FOCUS_ABSOLUTE`), VCMs on I²C (DW9714, DW9807/DW9817, AK7375, custom formats),
      move-time model, frame-exact `LensSchedule`, `FrameControls::lens`; IMX708 PDAF decoding
      from embedded data; software ISP focus statistics; PiSP CDAF noise from the noise profile;
      Styx controls `AF_MODE`, `AF_TRIGGER`, `AF_STATE`, `LENS_POSITION`, `AF_WINDOWS`,
      `AF_METERING`, `AF_RANGE`, `AF_SPEED`.
- [ ] Hardware validation on a Camera Module 3 (IMX708 + DW9817): the lens entity and its
      ancillary link appear as expected under `dw9807-vcm`; moves land on the frame
      `FrameControls::lens` reports settled (settle time 12 ms and `delay = 2` are guesses);
      the dioptre map (Raspberry Pi's 0 D → 445, 15 D → 925); the PDAF line's offset in the
      metadata buffer (two mode lines in, as libcamera) and the sign of `pdaf_gain`; CDAF
      figures of merit on the PiSP behave like the simulation's (peak width, noise); one-shot
      and continuous AF times against libcamera's (`rpicam-hello --autofocus-mode`).
- [ ] AF on a bridged sensor with an I²C VCM (no module here yet); the DW9807 busy flag is
      not polled.
- [ ] AF pause (libcamera's `AfPause`); PDAF from sensors other than the IMX708; the GPU ISP's
      focus statistics; CDAF windows on the PiSP follow the AF windows (today the 8×8 grid
      covers the frame and the windows weight its zones).

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
- [x] Built-in per-camera metrics, cheap enough to leave on (262 ns per frame on the CM5;
      PiSP path CPU unchanged within measurement): frames, measured vs configured fps, drops by
      cause (sensor gaps, queue overflow, corrupt, ISP skips), sensor-to-delivery/receive
      latency, ISP and worker CPU time, 3A state, restarts, buffers held and hold times, each
      consumer of a shared capture and each camera service client; `camera_metrics()`,
      `styx::metrics::snapshot()`, Prometheus text (+ `metrics-http`), serde, the camera
      service's metrics request, `metrics_top`. Checked against the consumers' own measurement
      on the CM5. See [docs/metrics.md](docs/metrics.md).

### Portability (`no_std`, phase 1: the brains)
- [x] `styx-core-rs` (formats, plane layouts, frame metadata, requirements, controls, SIMD
      kernels), `styx-algo` (all 3A and AF, tuning from TOML / Raspberry Pi JSON strings, the
      simulator), `styx-softisp` (kernels, `SoftIsp` on the calling thread), `styx-sensor`
      (descriptions, timing, gains, scheduler, embedded data, lenses, the driver over a
      `RegisterBus`), `styx-pisp` (config builders, tiling, statistics), `styx-dng` build as
      `no_std` + `alloc` without their default `std` feature; Linux builds unchanged (softisp
      benches within noise). Built for Cortex-M33, RISC-V and wasm32 in CI
      (`scripts/check-nostd.sh`); `examples/nostd-smoke` runs AE/AWB and the ISP without std
      as a host test. See [portability.md](docs/portability.md).

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
- [ ] Log or export `camera_metrics()` (or serve `styx::metrics::serve_http`) in
      helios-peripherals in place of the `health_report`/`metrics` logging every 30 frames.

### Image quality
- [x] Black level at high gain: not a black level difference. Zero-exposure levels through the
      bridge and through the `ov9282` driver agree within 0.1 code at 1-15.5× (BLC registers the
      same); the 2.4 codes were most likely the 50 Hz lamp. To close it against libcamera
      itself: its raw at a 1-line exposure and 8× (pipeline.md, "Quality vs libcamera").
- [ ] Verify the unverified kernel-sensor data files on real cameras (imx219, imx477, imx708, ov5647).
- [x] Camera calibration without libcamera's `ctt`: `styx-tune` (`crates/tune`, `tools/styx-tune`)
      calibrates black level (per channel and gain; hot pixels), lens shading per colour
      temperature, the AWB curve, colour matrices (chart found automatically or from corners),
      the noise profile, lux and GEQ from Styx MCAP / raw recordings or DNGs, writes Styx TOML
      and Raspberry Pi JSON (every libcamera tuning round-trips), and records the shots through
      Styx (`styx-tune capture`). Recovers a synthetic sensor's parameters (black within 0.02
      codes, shading tables 0.1%, CCM coefficients 0.011, noise 0.5%). See
      [docs/tuning.md](docs/tuning.md).
- [ ] OV9782 tuning of our own (today: the HeliOS tuning, identity colour matrix): a real
      session (ColorChecker, 3-5 high-CRI lights 2700-6500 K with a colour meter, diffuser,
      lux meter, lens cap; docs/tuning.md). From the CM5 without a chart so far: black level by
      gain (65.5/64.9 codes by row at 1×) and the temporal noise (slope 2.75 + 38 against the
      file's 5.38: `ctt`'s spatial estimate includes texture; its denoise strengths were tuned
      against it).
- [ ] Lux on the software ISP path: its statistics' luma is taken before white balance, the
      PiSP's (and `styx-tune`'s lux reference, as `ctt`'s) after; lux estimates differ by the
      white balance there.
- [ ] `styx-tune`: DNG input through `styx-dng` once merged (`input::RawDecoder`); defective
      pixel lists to the ISP; AWB priors from scene statistics; a covered-lens dark session on
      the OV9782 for the black level at high gain.

### Performance
- [x] PiSP: 0.12 ms/frame was the `pispbe` driver writing its whole config per job with a
      barrier per word. Patched driver (`kernel-modules/pispbe`: relaxed MMIO, only changed
      words, cached config copy): 117 → 8 µs per job, Styx API 0.35 → 0.23-0.26 ms/frame at
      30 fps (0.25 → 0.13-0.15 at 120), latency −0.11 ms, outputs bit-identical, libcamera
      unaffected. Installed on the dev box as an override of `pisp_be`.
- [ ] Extra back end passes run one after another, each waited for: queue them together (the
      raw buffer at more input slots, the TDN input buffer shared) to save ~0.03 ms of latency
      per pass. Pass buffers are allocated at start (`PispOptions::pass_buffers`); a camera
      service client joining with more regions than planned restarts the capture
      (`VIDIOC_CREATE_BUFS` could add them while streaming).
- [ ] Regions: scaled regions and regions in another format than the main output's
      (`PassSpec::size` / `format` exist) are not planned yet; a main-output crop that moves
      by more than half its size restarts the temporal average (a tracker's crop moving fast
      runs without temporal denoise on those frames).
- [ ] Ship the `pispbe` patch in the HeliOS image (kernel patch) and/or send it upstream (draft
      in `kernel-modules/pispbe/README.md`; ask Raspberry Pi whether the config registers are
      guaranteed to keep their values between jobs).
- [ ] Software ISP: outside the image maths only ~0.3 ms of memory traffic and ~0.15 ms of
      dequeue/sync/bookkeeping are left (staging copy and end-of-access sync gone). Integer path
      (Pi 4 class): colour matrix 1.1 ms and tone table 1.5 ms per frame on the A76; not yet
      measured on a real Cortex-A72. AVX-512 VBMI exact tone table (untested: no AVX-512 host).
- [x] PipeWire node copies each frame once: the node now allocates the PipeWire pool (memfds, and
      dma-bufs of them via `udmabuf` for DMA_DRM consumers) and the camera captures into it
      (`CaptureBuffers`: V4L2 DMABUF import, virtual camera) when frames pass through unchanged;
      a frame is held until the consumer returns its buffer. Copy otherwise (docs/ecosystem.md).
- [ ] PipeWire zero copy, remaining: run V4L2 DMABUF import against a driver that takes it (a UVC
      webcam; v4l2loopback is MMAP only); native PiSP outputs and libcamera capturing into caller
      buffers; converted routes decoding into the pool; `--service` frames (the service's own
      buffers); a CMA dma-heap pool for CSI receivers that need contiguous buffers.

### Platforms
- [ ] A second bridged sensor and a non-Pi board (software ISP or its own ISP).
- [x] `no_std` phase 2, steps 1-3 ([portability-design.md](docs/portability-design.md)):
      `styx-hal` (camera hardware traits, no `alloc`; embedded-hal 1.0 / embedded-hal-async 1.0
      as the bus vocabulary), the sensor driver over any embedded-hal I²C/SPI bus, blocking and
      async (`AsyncSensorDriver`), the `[bus]` section, descriptions compiled at build time
      (postcard), zero allocations per frame in the schedule and the sensor frame path
      (counting-allocator test); CM5 unchanged (first frame 34.4 vs 34.25 ms median, landing and
      register read-back identical).
- [x] `no_std` phase 2, steps 4-6 (`native/runtime`): `styx-runtime` (the sensor side,
      `Camera<P>`, frame stream and buffer leases, the sensor service; `no_std` + `alloc`),
      `styx-native` as its Linux platform (`V4l2Receiver` with the rp1-cfe handshake inside),
      the processing loop written once over `FrameIsp`/`InlineIsp` in a `no_std` pipeline core;
      CM5 unchanged (same syscalls and allocations per frame, CPU and latency within noise).
- [x] `no_std` phase 2, steps 7-8 (`work/runtime-78`): still and bracket decisions in the
      pipeline core (`still_runner`), metrics counters in the runtime (`styx_runtime::metrics`,
      `portable-atomic`), `styx` keeping the mechanics; `examples/nostd-camera`: `Camera` on a
      mock platform with the software loop, 3A, a bracket and the counters, built for the
      bare-metal targets, run on the host without `std` and bit for bit the same as over the
      `std` builds. CM5: bracket landed on consecutive frames, metrics overhead 270 ns before
      and after, CPU per frame within noise.
- [ ] The MCU port (`ports/stm32h7-dcmi`: swap `examples/nostd-camera`'s `board` for a DCMI
      receiver, I²C and a timer) and an `rkisp1` board; run on real MCU hardware. A linked
      firmware image (allocator, panic handler, `cortex-m-rt`) is not built yet: CI checks the
      crates and runs the logic on the host.
- [ ] `examples/nostd-camera` reprocesses stills inline on the superloop (a bracket costs three
      full-quality software passes between two frames); a firmware with an executor would run
      them in a low-priority task.
- [ ] Runtime follow-ups: the PiSP front end as a `Receiver` (`PispFeReceiver`; the external
      "driven" path still runs the event thread and `serve_sync` directly); the Linux sensor
      side and devices are `dyn` (one indirect call each per frame, measured free) — static
      dispatch if a profile ever shows it; `Receiver::poll_sync` on Linux is woken only at stop
      (the event thread serves frame starts itself).
- [x] Generic hardware on Lemnos 2.0 (`native/lemnos-switch`): register maps, `RegWrite`,
      error kinds (`lemnos-hal`), i2c-dev, GPIO and uevents (`lemnos-linux`), VCM drivers
      (`lemnos-drivers-vcm`), clock outputs; Styx's copies deleted (portability-design.md
      "Moved to Lemnos"). Lemnos fix on the way: i2c-dev transfers without allocation.
- [ ] Lemnos: Styx depends on a pinned commit of Lemnos `dev` by git (root `Cargo.toml`, `fuzz/Cargo.toml`);
      switch to crates.io 2.0 once it is published.
- [ ] Lemnos: `styx-hal`'s I²C mock (`MockI2c`) needs shared clones, async suspension and a
      dead target before `lemnos_hal::mock` can replace it; `styx::watch` (inotify on `/dev`)
      could use `LinuxHotplugWatcher`; the native provider's hotplug polls sysfs on a timer
      (uevent wake-ups through `lemnos_linux::uevent` would cut its latency).
- [ ] `native-pipeline regcheck`: `0x0101` and `0x1000` read back 0 after bring-up on the
      OV9782 (also on a178a44): write-only or self-clearing registers to exclude from the
      check, or a description issue; the documented "all 93 identical" no longer holds.
- [ ] `no_std` on targets without pointer-sized atomics (Cortex-M0, RISC-V without `a`):
      `SensorDriver` and the fp16 tables hold `Arc`s (`portable-atomic`, or `Rc`).
- [ ] `no_std` replays: 3A results through libm can differ from std's in the last bits, so a
      replay recorded on Linux is not bit-exact on a `no_std` target (deterministic per build).
      In `examples/nostd-camera`'s 90-frame run they were identical; the software ISP's `Auto`
      arithmetic is not (an x86 `std` build with AVX2 picks `IntPolyTone`, a `no_std` build
      picks by its compile-time features): runs that compare builds ask for `Int`.
- [x] GPU ISP path where Vulkan exists (`styx-gpuisp`, optional): the software ISP's pipeline
      as Vulkan compute shaders (ash, Vulkan loaded at run time), bit-exact with the integer
      arithmetic (pictures and statistics, RADV and llvmpipe), capture dma-bufs imported and
      outputs exportable, one submission per frame; `SoftLoop::use_gpu`, Styx feature
      `gpu-isp` with a planner cost. RX 6800 XT: 0.47 ms CPU per 1280x800 NV12 frame against
      1.77 ms for the software ISP on the same host, GPU 0.21 ms.
- [ ] GPU ISP on the Pi 5 (v3dv): not on the HeliOS image (needs Mesa's broadcom Vulkan
      driver, the loader, the v3d DRM driver); estimated slower than the A76 NEON path, so
      only to free the CPU. Untested on a real non-x86 GPU.
- [ ] GPU ISP: tables (0.2 ms per settings change: lens shading gain rows rebuilt with the
      gains, as the integer path does) could move the channel gains to the GPU at the cost
      of bit-exactness; outputs as exported dma-bufs through the Styx capture (today copied
      into heap frames).
- [x] Userspace UVC (optional): `styx-uvc` over usbfs, `BackendKind::Uvc` (feature `uvc`;
      `uvcvideo` stays the default where it has the camera). C270 on the CM5: YUYV and MJPEG
      at 30 fps, controls, replug, hotplug; PTS timestamps with no jitter (uvcvideo: 1.9 ms
      sd); ~0.5-1% of a core more CPU. See [docs/uvc.md](docs/uvc.md).
- [ ] Userspace UVC: a bulk / UVC 1.5 / SuperSpeed camera on hardware; status interrupt
      endpoint (control change events, button); still images; extension-unit controls.

### Stills and DNG
- [x] Still capture on a running capture (`capture_still` / `request_still` on `CaptureHandle`
      and `Frames`): JPEG/NV12/RGB/raw, DNG, fixed exposure, EV brackets on the frames the
      control schedule names, AE settle. CM5: preview 30.00 fps with 0 gaps while taking
      stills; still 95-134 ms request → ready, bracket of 3 on consecutive frames in ~340 ms
      (docs/stills-and-dng.md).
- [x] `styx-dng`: DNG 1.4 writer (calibration from the tuning, GainMap lens shading, EXIF,
      preview) and reader (camera DNGs: lossless JPEG tiles, packed samples); LibRaw renders
      Styx's DNGs.
- [ ] Run Adobe's `dng_validate` on Styx's DNGs (not available on the dev host).
- [ ] V4L2 stills at full resolution without a reconfigure; UVC still-image methods (still
      probe/commit, the still trigger); stills on a capture without a processed native mode
      leave the consumer one frame short.
- [ ] Still JPEG encoding: the `image` crate takes most of the ~45 ms still thread time at
      1280x800; turbojpeg (C) is faster where it is allowed.
- [ ] A per-camera DNG `NoiseProfile` from the tuning's noise model.

### Metrics
- [ ] Producer-side metrics for libcamera, file, replay, netcam and simulation captures (today
      counted as received); hold times for V4L2 and UVC buffers.
- [ ] Per-consumer hold times for in-process consumers of shared captures (today per capture
      buffer, and per camera service client); attribute a service client's queue drops to the
      client (they show on its consumer row of the camera).
- [x] AF in the metrics: state, mode, lens position (dioptres), lens settled, scans started
      (unit-tested; no camera with a lens on the dev box yet).
- [ ] CPU of the software ISP's worker pool per capture.

### Fuzzing
- [x] cargo-fuzz targets for every parser of untrusted bytes (UVC descriptors and streams, sensor
      descriptions, embedded data and kernel-driver reports, tuning files, PiSP statistics,
      kernel messages, raw and MJPEG decoders, imported frame layouts, frame socket and camera
      service messages and requests, recordings, raw recordings), `scripts/fuzz.sh`, nightly CI
      smoke run; see [docs/fuzzing.md](docs/fuzzing.md). Found and fixed: out-of-bounds reads in
      odd-width UYVY/NV12 decoding, SIGBUS on a short memfd from another process, unchecked
      imported descriptors, allocations sized from headers, overflow panics.
- [ ] Fuzz the netcam multipart parser (network input; only unit tests today) and the
      libcamera/V4L2 control and metadata conversions.
- [ ] The raw decoders still index with `get_unchecked` behind length checks (one was wrong);
      replace with checked slices where it costs nothing measurable.
- [ ] Longer runs (hours) and on AArch64 (the NEON paths are not fuzzed on x86).

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

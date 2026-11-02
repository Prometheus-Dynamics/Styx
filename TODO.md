# Styx native stack: progress and TODO

Styx as a stand-alone camera stack: USB and CSI cameras with nothing but the Linux kernel
underneath. No libcamera, no `v4l` crate, no per-sensor kernel driver. Design and device rules:
[docs/native-stack/README.md](docs/native-stack/README.md).

Audited against the code at 475e07a (2026-11-01). Numbers are from the CM5 / Raze with the OV9782
at 1280x800 unless stated. Every open item has *what* (its bold title), *why*, *done when*,
*blocked by* and *numbers*; "written, untested on board" means the code exists and only unit
tests or QEMU have run it. Done items move to [Done](#done) with their commit.

The user alone decides when a release is cut and tagged, and what blocks one. Versions stay on
the dev scheme (`2.0.1-dev` now; only the patch moves); the next release will be 3.0. Board work
gets tested on boards before anything ships, unless the user says otherwise. **Held** and
**paused** items wait for the user. Accelerators are chosen through plain `StyxConfig` knobs per
device, with a CPU fallback for each; CPU SIMD stays detected, as it changes no output.

## Open: the 2026-10-27 freeze list

- [ ] **Anti-flicker verified at 120 fps** (paused: the user paused bench tests). *Why:* AE chased
      100 Hz flicker at 120 fps under mains lamps. *Done when:* `flicker_check` on the CM5/Raze
      under a 50 Hz lamp at 120 fps: AE locks and the output frame-to-frame spread stays under
      5%, with flicker avoidance and deflicker on (the defaults). *Blocked by:* the pause and a
      mains lamp at the bench in a Raze slot. *Numbers:* room lamp at 120 fps, 15-18% → 3.4-4.7%
      spread; not yet the checked run this item asks for.
- [ ] **YUYV no-frames fallback: the bench check** (paused: bench tests). *Why:* a UVC camera that
      negotiates YUYV and delivers nothing hung a client. The fallback and the cause in the error
      are in (302b1f6, 9cb7009: `planner/stalled.rs`; unit tests in `planner/usb_tests.rs`).
      *Done when:* the C270 on the CM5 in a YUYV mode it cannot sustain falls back to MJPEG and
      the error names the cause. *Blocked by:* the pause, and the C270 plugged back in.
      *Numbers:* none: unit tests only.
- [ ] **OV9782 tuning of our own** (held until the user has a chart). *Why:* the imported tuning has
      an identity colour matrix. *Done when:* a `styx-tune` session (ColorChecker, 3-5 lights
      2700-6500 K) produces the TOML and colour error drops against the chart. *Blocked by:* the
      user's ColorChecker kit and time. *Numbers:* from the CM5 without a chart: black level by
      gain (65.5/64.9 codes by row at 1×), temporal noise slope 2.75 + 38 against the file's 5.38
      (`ctt`'s spatial estimate includes texture).

## Open: hardware support

Every item here is written, untested on board, or not written where it says so. No Rockchip or
K230 board is at the bench, so none has numbers yet; each records CPU per frame, latency and
memory when it runs.

- [ ] **K230D: raw capture and the RVV software ISP on the board.** *Why:* the K230D camera path; our
      own 3A and tuning on the OV9782. *Done when:* the OV9782 over CSI streams 1280x800 at 30 fps
      through `soft_worker` with 3A converged; `grep isa /proc/cpuinfo` shows `v` and
      `SimdBackend::Rvv` is reported; CPU per frame and memory recorded. *Blocked by:* a K230D
      board and Gaia's image (Linux-only SDK on the big core). *Numbers:* none on hardware; the
      RVV leaves pass under QEMU at VLEN 128.
- [ ] **K230D: the ISP8000 with our own 3A** (user decision; `styx-k230isp`, 10617c6, written,
      untested on board). *Why:* the hardware ISP frees the vision core and its scaled channels
      make pyramid levels; the RVV software ISP is the fallback (the ISP knob picks). *Done when:*
      the OV9782 streams through the ISP8000 at 1280x800 30 fps with Styx's 3A converged; `pyramid:
      3` comes from the scaled channels (1/8 from the smallest) within a stated tolerance of the box
      filter; the register programming is checked against dumps from the board (today: unit tests
      against a fake register file). *Blocked by:* a K230D board.
- [ ] **K230D: VPU encode (H.264/H.265/JPEG).** Not written for the K230: `V4l2M2mEncoder` is generic
      and unproven against `mvx`. *Why:* preview and recording without the CPU. *Done when:* the M2M
      backend drives `/dev/video0` (`mvx`) at 1280x800 30 fps with dma-buf NV12 input, and the CPU
      fallback is used without it. *Blocked by:* a K230D board.
- [ ] **K230D: 2D unit scaling.** Not written for the K230: `V4l2M2mConverter` is generic. *Why:*
      preview frames and levels without the CPU. *Done when:* `/dev/video5` (`nonai-2d`) scales
      through the M2M backend within a stated tolerance. *Blocked by:* a K230D board.
- [ ] **RK3588S: ISP30 bring-up** (written, untested on board). *Why:* the Rockchip HVS boards. *Done
      when:* the OV9782 through rkcif and the ISP streams NV12 at 120 fps with 3A converged; entity
      names (`media-ctl -p`) and the fixed-point constants (gains, CCM, LSC, statistics bit
      depths) confirmed. *Blocked by:* an RK3588S board on the vendor 6.1 BSP.
- [ ] **Rockchip: rkcif graph per chip** (`IspDevice` sets up one graph, written, untested on board).
      *Why:* capture feeds the ISP. *Done when:* each chip's (RK3588S, RK3576, RK3566, RK3562,
      RV1106) sensor-to-ISP media graph is set up by Styx and streams. *Blocked by:* boards.
- [ ] **Rockchip: the ISP's self path for pyramid level 1 and preview.** Not written: `IspDevice` has
      no second output, and whether the media core takes a second enabled link from the ISP's
      source pad is unverified (docs/rockchip.md). *Why:* a second scaled output costs nothing.
      *Done when:* level 1 comes from the self path, deeper levels from it, within a stated
      tolerance of the box filter. *Blocked by:* an RK3588S.
- [ ] **RK3566: rkisp ISP21** (`styx-rkisp` parameters and statistics, 0dad505, written, untested on
      board). *Why:* user decision, both low-cost chips. *Done when:* the chip streams the OV9782
      with 3A converged. The structures, builder and parser have tests; the fixed-point constants
      and vertex layout are the headers' silence, to confirm on the chip. `styx_pipeline::rkisp`
      passes `Generation::Isp30` only, so the pipeline side is still to write. *Blocked by:* for
      that, nothing; for the run, an RK3566 board.
- [ ] **RK3562: rkisp ISP32-lite** (parameters and statistics, e9adfc0, written, untested on board).
      *Why:* user decision, both low-cost chips. *Done when:* as RK3566, the pipeline side included:
      nothing selects `isp32` yet. *Blocked by:* for that, nothing; for the run, an RK3562 board.
- [ ] **RK3576: rkisp, its generation.** Not written: the docs list it as a fallback, its generation
      is unconfirmed. *Why:* a supported chip. *Done when:* the generation is confirmed from the
      BSP's rkisp hardware table, then as RK3566. *Blocked by:* an RK3576 board and BSP.
- [ ] **Rockchip: RGA scale, crop and convert** (RGA3 on RK3588S, RGA2 on the others). *Why:* deeper
      pyramid levels and preview frames without the CPU. Mainline's `rockchip-rga` is an M2M device
      `V4l2M2mConverter` drives; the vendor BSPs ship multi-rga, whose `rga_req` layout is only in a
      GPL-2.0 header. One binding for the stack, in Daedalus; Styx adds no ioctl layer. The scaler
      knob covers ISP scalers until this adds an M2M stage to the planner. *Done when:* the
      conversion stage picks RGA through the accelerator knob, scaling within a stated tolerance,
      CPU fallback without it. *Blocked by:* Daedalus's RGA backend (RGA3); a Rockchip board.
- [ ] **Rockchip: MPP / rkvenc H.264/H.265 and the JPEG encoder** (not written; stateless VPUs, Hantro
      and rkvenc, too: `V4l2M2mEncoder` is stateful only). *Why:* preview and recording without
      turbojpeg on the CPU. *Done when:* a `styx-codec` backend over librockchip_mpp encodes dma-buf
      NV12 from the ISP at 1280x800 120 fps, and the CPU fallback is used without it. *Blocked by:*
      a Rockchip board on the vendor BSP (`mpp_service`).
- [ ] **RV1106: pyramid levels from the ISP.** Not written: `rkisp_worker` makes no pyramid companion.
      *Why:* Eidos on Flame spends the most Styx time on the CPU pyramid downscale. *Done when:* the
      rkisp's second output feeds `CompanionKind::Pyramid` like the PiSP's output 1 (selfpath scaler,
      or the vendor 4x4/8x8 sampling outputs), luma or NV12's Y plane at 256-byte strides; `box2_row`
      stays the fallback. *Blocked by:* a Flame board and its BSP.
- [ ] **Flame (RV1106G2): bring-up** (ISP32 parameters and statistics written, untested on board;
      pipeline selection as RK3562). *Why:* the Flame board. *Done when:* rkisp, rkcif graph and
      rkvenc run the OV9782 within the RV1106G2's memory, with the CPU fallbacks. *Blocked by:* a
      Flame board and its BSP.
- [ ] **Queued PiSP passes on the Raze** (220aff5, a083112: written, the detection untested on board).
      *Why:* `pass_jobs` 0 now means all queued when the driver has `queue_safe`, 1 otherwise, and
      that has not run. *Done when:* `/sys/module/pisp_be/parameters/queue_safe` reads `Y` on the
      Raze with the patched module and a capture resolves `pass_jobs` to `QUEUED_JOBS`, 0 "not safe"
      messages. *Blocked by:* HeliOS's image bump. *Numbers:* with the module and `--pass-jobs 4`
      (a62f285): 0 messages, no hang, 0.251 vs 0.276 ms of pass time per frame, latency p50 8.53 vs
      8.56 ms, CPU 0.1 ms either way.
- [ ] **pispbe patch: carried** (user decision: we keep carrying it; the drafts in
      `kernel-modules/pispbe/README.md` stay unsent, nothing gets filed). *Why:* it takes the config
      write from 117 to 8 µs per job and makes queued jobs safe; the stock driver overwrites a held
      job. A kernel bump stops the image build until the patch is redone. *Done when:* the user
      decides otherwise; until then this is a standing cost, not work. *Blocked by:* the user.
      *Numbers:* Styx API 0.35 → 0.23-0.26 ms/frame at 30 fps; stock driver 455 "not safe" in 5 s.
- [ ] **CM5 HEVC decode check** (`hevc_v4l2request` through FFmpeg). *Why:* HEVC recordings only; Styx
      records MJPEG and raw. *Done when:* a recording decodes through `rpi-hevc-dec`. *Blocked by:*
      an Atlas image with that driver; HeliOS says when. *Numbers:* none: the driver is not in the
      image.

## Open: every stage on every backend, and allocations

- [ ] **Every stage accelerated on every backend** ([docs/acceleration.md](docs/acceleration.md)).
      *Why:* user rule: every stage that can be accelerated is, on x86, NEON64, armv7 NEON, RVV, the
      GPU and each board's fixed-function units; losing variants stay behind knobs. *Done when:* the
      matrix has no gap cells; each leaf conformance-tested against scalar (guard bytes, odd widths)
      in the gate, on hardware or QEMU. *Blocked by:* nothing for the CPU and GPU columns; boards
      for the fixed-function rows. *Numbers:* recounted from the matrix: **213 pass, 44
      gaps, 1 n/a of 258 cells** (43 stages). Gaps by column: x86 3, NEON64 2, NEON32 5, RVV 9, GPU 25.
      By stage: the GPU column of `post.rs` (5), YUYV → NV12 chroma (5), edge-directed demosaic (5),
      armv7/RVV NV12, YUYV and I420 → RGB (6), the GPU column of the packed conversions (11) and of
      those three → RGB (3), polynomial tone GPU (1), fp16 x86/RVV/GPU (3), JPEG/H.264 RVV (3), JPEG GPU
      (2). Fixed-function: 5 pass, 7 gaps, 6 of which need a board. armv7 and RVV leaves cover every
      kernel in `styx_core::simd` and the integer kernels of `styx_softisp::simd` except the
      edge-directed demosaic.
- [ ] **Software ISP: AVX-512 VBMI exact tone table.** Not written (no AVX-512 code in the tree; the
      old entry read as written, untested). *Why:* the fastest exact table on such hosts. *Done
      when:* a leaf passes the conformance tests. *Blocked by:* an AVX-512 host to run it.
      *Numbers:* none. Beside it: colour matrix 1.1 ms and tone table 1.5 ms per frame on the A76's
      integer path; never measured on a real Cortex-A72.
- [ ] **Zero steady-state allocations: what isn't counted, or isn't 0.** *Why:* standing rule for
      every dependency: a count of 1-3 per frame is a finding; assert 0, or state the physical reason.
      *Done when:* every per-frame path below asserts 0, or names its reason. *Blocked by:* a device
      for the worker loops; nothing for the rest. *Numbers:* asserted 0 (6a6c40d): PiSP lease with and
      without a companion, UVC frame lease, GPU ISP frame from memory, PiSP processed path on a host
      (`pisp_alloc`). Also asserted 0: software ISP output lease and rkisp lease
      (`frames_allocate_nothing`, `BackingPool`); a `SoftIsp` frame at 1 and 4 threads, plain and post
      stages, RGB24/NV12/I420, and with statistics when the caller uses `recycle_stats`
      (`crates/softisp/tests/alloc.rs`), also with white balance, gain, lens shading and tone changing
      every frame (`swap_params`, Int, IntPolyTone and Half). The software ISP loop (`SoftLoop`, the
      algorithms included) on a host when its outputs are handed back (`SoftLoop::recycle`): 0.00 per
      frame over 300 frames, 176 with the algorithms and 124 settled ones without (`soft_alloc`; was
      129.83). **Not 0:** the replay source (`replay_isp.rs`) copies each frame into a new NV12 buffer
      and lease. **Not counted:** V4L2 zero-copy (uses the pool, its loop needs a device), PiSP raw
      and sliced leases, GPU ISP `Input::Buffer` and its statistics, the raw native lease
      (`native_backend.rs`), libcamera, and every real worker loop on a board.
- [ ] **GPU import pad from Daedalus.** *Why:* `gpu_import_pad` is a fixed 256 B; Daedalus knows the
      device's need. *Done when:* with a Daedalus context the pad comes from
      `GpuFrameImports::required_len(end)` / `GpuStorage::dmabuf_import_len(gpu, len)`, 256 otherwise.
      *Blocked by:* nothing. *Numbers:* v3dv needs frame + 256 B; 1280x800 GREY pads to 1 232 896 B.
- [ ] **GPU ISP on the Pi 5: timing** (import and output verified on the Raze's v3dv at eeefd6d:
      dma-buf import and export pass, Daedalus buffer path and equivalence bit-exact, 0 validation
      messages). *Why:* an estimate is all we have (`crates/gpuisp/README.md` and
      docs/native-stack/pipeline.md say so). *Done when:* ms per 1280x800 frame and CPU measured
      there and written into both. *Blocked by:* a Raze slot. *Numbers:* no frame
      timing; estimated 4-8 ms GPU against 2.6-3.0 ms fp16 NEON. `styx-wgpu`'s pyramid on the same
      GPU: 1.67 ms p50 and 0.38 ms CPU for 3 levels (42fb407).
- [ ] **GPU ISP: outputs through the capture, and the tables.** *Why:* `GpuIsp` exports dma-bufs
      (869416a) but the capture copies them into heap frames; the 0.2 ms table rebuild per settings
      change could move to the GPU at the cost of bit-exactness. *Done when:* the capture hands out the
      exported dma-buf with `CopySite` zero; the table variant measured behind a knob. *Blocked by:*
      nothing. *Numbers:* RX 6800 XT 0.47 ms CPU, 0.21 ms GPU per frame; tables 0.2 ms.
- [ ] **Zero copy: Daedalus importing software ISP frames on the CM5.** The output path is written and
      in (8b1daa4, 93ed532). *Why:* the point of dma-heap outputs. *Done when:* Daedalus reads those
      frames on the CM5 without a copy. *Blocked by:* a Raze slot and Daedalus's side.
      *Numbers:* none on the board for this path.
- [ ] **Userspace UVC: a bulk / UVC 1.5 / SuperSpeed camera on hardware.** The single-transfer lease
      is written, untested on a camera (475e07a). *Why:* only the C270 (isochronous) has run. *Done
      when:* a bulk camera's frames reach consumers with `CopySite::Capture` at zero. Isochronous
      copies stay (a header per packet, as `uvcvideo`). *Blocked by:* a bulk UVC camera. *Numbers:*
      none on hardware; unit test `a_whole_frame_in_one_transfer_keeps_its_buffer`.
- [ ] **PipeWire zero copy, remaining.** *Why:* the node copies unless frames pass through unchanged.
      *Done when:* V4L2 DMABUF import runs against a driver that takes it (a UVC webcam; v4l2loopback
      is MMAP only); native PiSP outputs and libcamera capture into caller buffers; converted routes
      decode into the pool; `--service` frames; a CMA dma-heap pool for CSI receivers. *Blocked by:*
      a UVC webcam for the first; nothing for the rest. *Numbers:* none.

## Open: release mechanics (when the user calls it)

- [ ] **Public API review against 2.0.** *Why:* the next major is the chance for breaking changes.
      *Done when:* a `cargo public-api` diff against 2.0.0 is reviewed, every removal is in the
      CHANGELOG with its replacement, and HeliOS, PhotonVision and photon-image-modifier build against
      it. *Blocked by:* nothing. *Numbers:* not run yet.
- [ ] **Version, CHANGELOG, gates at the release rev, tag and announce.** *Why:* the release itself.
      *Done when:* every crate carries the release version, Unreleased becomes it with a date and a
      migration section, `scripts/gate.sh` and the Raze sweep pass at the tagged rev, the tag is
      pushed, and HeliOS, Eidos and PhotonVision are told the rev. *Blocked by:* the user's call.
      *Numbers:* `2.0.1-dev` today. Gate lanes at 475e07a, rerun on a quiet machine 2026-11-01: perf-smoke passes with every metric well inside its limit (rotate90 p95 0.36 ms against 10; the
      15-23 ms it showed earlier was machine load, 80-90, not the code); riscv and armv7 lanes pass. The
      `cross` lane has no linker on the dev PC.

## Open: backlog

Not on the freeze list. *Numbers:* none measured unless stated.

- [ ] **Regions, remaining.** *Why:* gaps in the region paths. *Done when:* consumers sharing a
      software-ISP capture get their regions beyond views (today luma only); a true 4x4 software
      overview bin beyond 2; libcamera regions of another size without scaling to the first
      (reconfigure, or a second crop stream); binned software modes crop (the GPU ISP does, 21e1bf6).
      *Blocked by:* nothing.
- [ ] **AF on real hardware: Camera Module 3** (IMX708 + DW9817; AF is simulation-only). *Why:* settle
      time 12 ms and `delay = 2` are guesses. *Done when:* lens entity and ancillary link appear under
      `dw9807-vcm`; moves land on the frame `FrameControls::lens` reports; dioptre map (0 D → 445, 15 D
      → 925), PDAF line offset and `pdaf_gain` sign confirmed; CDAF behaves as simulated, including the
      windowed grid (2a5ccda, written, untested on board); AF times against `rpicam-hello`. *Blocked
      by:* a Camera Module 3. *Numbers:* simulated one-shot 9 frames, PDAF 2.
- [ ] **AF on a bridged sensor with an I²C VCM.** *Why:* untested combination; the DW9807 busy flag is not
      polled. *Done when:* it focuses on one. *Blocked by:* a module.
- [ ] **PDAF beyond the IMX708; GPU ISP focus statistics.** *Why:* `crates/gpuisp/src/stats.rs` returns
      none, so AF has no contrast there. *Done when:* the GPU ISP reports them bit-exact, and a second
      sensor's PDAF decodes. *Blocked by:* a sensor for PDAF; nothing for the GPU.
- [ ] **Verify the kernel-sensor data files on real cameras** (imx219, imx477, imx708, ov5647: `verified =
      false`). *Why:* written from drivers, never run. *Done when:* each streams and is marked verified.
      *Blocked by:* the cameras.
- [ ] **`styx-tune`: defective pixel lists, AWB priors, dark session.** *Why:* hot pixels only turn DPC on
      today; priors come from nothing. *Done when:* pixel lists reach the ISP, priors come from scene
      statistics, and a covered-lens session gives the OV9782's black level at high gain. *Blocked by:*
      the bench pause for the dark session.
- [ ] **A second bridged sensor and a non-Pi board.** *Why:* only the OV9782 on the CM5 has run bridged.
      *Done when:* one of each streams. *Blocked by:* hardware.
- [ ] **The MCU port.** Not started: no `ports/` directory. *Why:* the footprint images are linked and
      sized, never run. *Done when:* `ports/stm32h7-dcmi` swaps `examples/mcu-footprint`'s `board` for a
      DCMI receiver, I²C and a timer, with cycle counts per frame on a core; an `rkisp1` board too.
      *Blocked by:* an MCU board. *Numbers:* Cortex-M7 flash A 10.9, B 89.4, C 128.2, D 205.6 KB.
- [ ] **MCU follow-ups** (docs/mcu.md). *Why:* size and speed left on the table. *Done when:* each is
      decided: postcard decoding (16 KB of every image with a sensor) against generated constructors; 3A
      in `f64` soft-float, speed measured; brackets holding every shot's raw frame; panic messages (needs
      nightly `build-std`); ESP32-S3 build; a compiled tuning needs a serde form without
      `skip_serializing_if`. *Blocked by:* an MCU board for speed.
- [ ] **`examples/nostd-camera` stills in a low-priority task.** *Why:* a bracket costs three full software
      passes between two frames on the superloop. *Done when:* a firmware with an executor runs them
      in a task. *Blocked by:* nothing.
- [ ] **Runtime follow-ups.** *Why:* the PiSP's external "driven" path still runs the event thread and
      `serve_sync` directly; `Receiver::poll_sync` on Linux wakes only at stop. *Done when:*
      `PispFeReceiver` is a `Receiver` (designed in docs/portability-design.md, not written). Static
      dispatch only if a profile shows it (`dyn` measured free). *Blocked by:* nothing.
- [ ] **Lemnos from crates.io.** *Why:* root `Cargo.toml` and `fuzz/Cargo.toml` use the `dev` branch by git.
      *Done when:* switched to 2.0 from crates.io. *Blocked by:* Lemnos publishing it.
- [ ] **Native provider hotplug on uevents.** *Why:* `crates/native/src/provider.rs` polls sysfs every second;
      `styx::watch` already uses `lemnos_linux::uevent`. *Done when:* the provider does too. *Blocked by:*
      nothing. *Numbers:* up to 1 s latency.
- [ ] **`native-pipeline regcheck`: `0x0101` and `0x1000` read back 0.** *Why:* docs/portability-design.md
      still says all 93 identical. *Done when:* they are excluded as write-only or self-clearing, or the
      description is fixed. *Blocked by:* a Raze slot to confirm. *Numbers:* 2 of 93 differ.
- [ ] **`no_std` replays across builds.** *Why:* libm can differ from std in the last bits, and `Auto` picks by
      build. *Done when:* documented comparison runs ask for `Int`, or the results are shown equal. *Blocked
      by:* nothing. *Numbers:* identical over `nostd-camera`'s 90 frames.
- [ ] **Stills, remaining.** *Why:* V4L2 stills reconfigure; UVC still-image methods are missing; stills on
      a capture without a processed native mode leave the consumer one frame short. *Done when:* each
      is gone. *Blocked by:* a camera with still methods for UVC.
- [ ] **Still JPEG encoding numbers.** *Why:* the default features fall back to the `image` crate. *Done when:*
      the ~45 ms still thread time is re-measured with turbojpeg. *Blocked by:* a Raze slot. *Numbers:*
      ~45 ms, before turbojpeg.
- [ ] **DNG: `dng_validate` and a per-camera `NoiseProfile`.** *Why:* only LibRaw has read our DNGs; no
      `NoiseProfile` is written. *Done when:* Adobe's validator passes and the tag comes from the tuning's
      noise model. *Blocked by:* the DNG SDK for the validator.
- [ ] **Metrics, remaining.** *Why:* file, replay, netcam and simulation captures count as received; UVC,
      V4L2 and libcamera buffers have no hold times (their leases are not tracked); in-process consumers
      of a shared capture have no hold times of their own; a service client's queue drops show on its
      consumer row; no CPU for the software ISP's worker pool. *Done when:* each reports. *Blocked by:*
      nothing.
- [ ] **Fuzz the rest.** *Why:* the netcam multipart parser is network input with unit tests only; libcamera
      and V4L2 control conversions and the rkisp and K230 statistics have no target. *Done when:* each has
      one. *Blocked by:* nothing. *Numbers:* 16 targets today.
- [ ] **Raw decoders' `get_unchecked`.** *Why:* the wrong one was fixed in 1bf166d. *Done when:* checked
      slices wherever it costs nothing measurable. *Blocked by:* nothing. *Numbers:* 104 calls.
- [ ] **Longer fuzz runs, and on AArch64.** *Why:* NEON paths are not fuzzed on x86. *Done when:* hours-long
      runs on both. *Blocked by:* an AArch64 host for hours.
- [ ] **rp1-cfe device-tree node leak** (upstream). *Why:* one node per runtime overlay up/down; dev runtime
      path only. *Done when:* fixed upstream. *Blocked by:* upstream.
- [ ] **Deflicker corners.** *Why:* rolling-shutter band gains are unverified on a sensor (needs `readout` from
      its description); 30 fps with short exposures leaves ~2%; with deflicker off, a clipped lamp makes AE
      chase the beat at 120 fps (simulated). *Done when:* verified on a sensor, or accepted by the user.
      *Blocked by:* the bench pause. *Numbers:* as stated.
- [ ] **Reject impossible frame-length reports.** *Why:* a value from embedded data is trusted even when
      impossible (VTS 4 on an 800-line mode). *Done when:* the control schedule rejects such reports.
      *Blocked by:* nothing.

## Done

Numbers are as of the commit named.

### Since the last audit
- [x] **UVC bulk: a whole frame in one transfer keeps its buffer** (475e07a): the URB's buffer becomes
      the frame, the URB goes back with an empty pool buffer. Written, untested on a camera (open
      above). `UvcFrame::data` is now `FrameData`.
- [x] **No allocation per frame in a lease** (082be65 counted, 6a6c40d removed): `BackingPool`, companion
      free list. What it does not cover is open above.
- [x] **Accelerator knobs** (450cebc): `StyxConfig::accelerators` / `FrameRequest::accelerators`, `isp`,
      `encoder`, `scaler`, `pyramid`, each `Auto`, `Off` or `Required`; planner and native capture honour
      them, a camera service applies its config's to its clients.
- [x] **Queued PiSP passes** (a94268b, f08a8b5, 8da823e, a62f285, a083112, 220aff5): they wedged the patched
      `pisp_be` with fixed indices; the module now defers a job while the hardware holds one and exports
      `queue_safe`; Styx queues only then. Pass buffers are allocated at start; a client joining with more
      regions than planned restarts the capture. The board check is open above.
- [x] **GPU ISP imports on v3dv** (eeefd6d), **on Daedalus's device** (e0b47ec); **room past the frame for
      imports** (e5d704d, c79726d, 115bc66, a43c149, a8fa187): every dma-heap buffer is `padded_len`.
- [x] **Software ISP output as dma-bufs, allocation-free but its lease record** (8b1daa4, 93ed532).
- [x] **Rockchip main path handed out as dma-bufs** (a1831bc, written, untested on board); the copy stays as
      the fallback when fewer than two buffers would stay with the driver.
- [x] **Every pyramid level from the PiSP** (dc570e8), **PiSP focus statistics follow the AF windows**
      (2a5ccda): both written, untested on board. The old "CDAF windows" item was stale.

### Hardware support and mechanics
- [x] **Typed control-menu entry** (45cb3a3; HeliOS aa44c4d3 shows `MenuEntry` in its API and
      UI). Every menu control reports typed entries (named items and integer menus), set and
      read back by entry, tested per backend.
- [x] **libjpeg-turbo on RISC-V and HeliOS's feature sets in the riscv gate lane** (2fc1119).
      *Why:* the K230D could not build HeliOS (`require-simd` on a target without SIMD).
      *Done when:* `scripts/check-riscv.sh` lints `facade, metrics-serde, preview,
      codec-turbojpeg, replay-mcap, replay-mcap-lz4, replay-softisp, replay-png` and `native,
      v4l2, raw-decoders, serde, hotplug, async, codec-turbojpeg` with their C deps: passing.
      *Blocked by:* nothing.
- [x] **RVV pyramid downscale** (2fc1119). *Why:* the box downscale was ~48% of Eidos's
      RVV-lane instructions. *Done when:* `box2_row` has an RVV kernel identical to scalar,
      tested under QEMU at VLEN 128 and 256: passing. *Blocked by:* nothing.
- [x] **V4L2 M2M encoder and scaler backend** (f165f4b; delta flags fixed in this commit).
      `V4l2M2mEncoder` and `V4l2M2mConverter`: `vim2m_converts_frames` and
      `vicodec_encodes_frames` pass against the PC's `vim2m` and `vicodec` (2026-10-31). (00c3fd3)
- [x] **API cleanup and duplicate merge** (b1af88a, b6444d3, fbe003e, a24ec85, 373d703).
      *Why:* the freeze's cleanup pass. *Done when:* duplicate code merged and duplicate names
      removed: done; only trait-impl repetition remains. *Blocked by:* nothing.

### Foundations and stand-alone
- [x] Pure-Rust kernel interfaces (`styx-kernel`): V4L2, media controller, subdevs, events,
      dma-heaps (I²C, GPIO and uevents moved to Lemnos 2.0, `lemnos-linux`). (df84c54, 7fe3448, 6d1d4a0)
- [x] V4L2 backend on `styx-kernel`; the `v4l` crate is gone everywhere. (d23196a)
- [x] libcamera fully optional: no default build pulls it, CI job without it, checks in
      `scripts/check-feature-combinations.sh`. (97aac59)

### Sensors
- [x] Sensors as data (`styx-sensor`, TOML descriptions) driven from Rust through one generic
      kernel module, `styx-sensor-bridge`. (31bacba, 9c4f745)
- [x] Sensors with an upstream kernel driver run natively too (subdev controls, DelayedControls-style
      scheduling); data files for ov9281/ov9782 (verified), imx219, imx477, imx708, ov5647
      (unverified). See [adding-a-camera.md](docs/native-stack/adding-a-camera.md). (c76a6aa)
- [x] Frame-exact typed controls; exposure/gain land on the predicted frame, verified by embedded data. (9d91830, 0470c85)
- [x] Exact frame rates (VTS + 1 lines); 30/60/120 fps. (f4db779)
- [x] Burst I²C writes, 400 kHz cam0 bus, parallel bring-up: open → first frame 29 ms
      (libcamera 131–142 ms). (4b003cb, 635acdc)
- [x] Robustness: restart with held frames, error paths end clean, supervised reconnect, kill -9
      recovery, rmmod refused while streaming, `down.sh` never unbinds rp1-cfe under a user
      (that was the cause of the unexplained kernel oopses/reboots), 2×15 min soak with no leaks. (85a2723, 6e561ca, ff17553, 7dddf3d)

### Pi ISP and algorithms
- [x] PiSP front end statistics + back end processing, dma-buf hand-off, both outputs, pyramid level. (eef15ab, c89618f, b225561)
- [x] 3A in Rust (`styx-algo`): AGC, AWB (Bayesian, made continuous + hysteresis), adaptive ALSC
      (ported, matches libcamera to 1e-12), CCM, contrast, denoise/sharpen from tuning, TDN. (ce60331)
- [x] AE locked at frame 6 (229 ms from open; libcamera 664–675 ms); warm restarts lock by frame 2. (1b94a8e)
- [x] PiSP path CPU 0.26 ms/frame through Styx (0.8 % of a core; libcamera 2.9 %). (72bec55, 5c57f59)
- [x] Regions of interest from the PiSP for every consumer (`FrameRequest::regions`, up to 16
      each): the main output's crop, the second output's, extra back end passes over the same
      raw frame (0.03 ms + 1.35 ns/pixel of back end time, ~0.03 ms CPU per pass; 4 regions
      0.385 ms CPU and 9.1 ms latency per frame against 0.29 ms / 8.9 ms for one), regions
      paired with their frame (companions), temporal denoise read but not written by passes,
      `skip_stale_regions`; overview box-filtered without a PiSP. See
      [frame-planning.md](docs/frame-planning.md#region-of-interest). (b640362, 492a7eb)
- [x] Image quality matches libcamera within libcamera's own session-to-session spread. (d607dde)
- [x] Flicker avoidance (`Flicker::Auto`, the Styx default; `AE_FLICKER_MODE`): AE fits the
      lamp's flicker (harmonics of 50/60 Hz mains) from the frames and meters against the
      mean light, so 120 fps no longer chases it (exposure × gain spread 10% → 1%, cold start
      never locked → frame ~21 under the room's ±20% 50 Hz lamp); long exposures whole mains
      periods (30 fps: frame spread 2.4% → 0.2%); detects the mains frequency itself. (30b2826)
- [x] Deflicker (`Deflicker::Auto`, on with flicker avoidance; `AE_DEFLICKER_MODE`): each
      frame's ISP digital gain divided by the brightness the flicker model predicts for that
      frame (mains frequency tracked, confidence-gated, faded, headroom only where highlights
      would clip): output frame-to-frame spread under the room's lamp 15-18% → 3.4-4.7% at
      120 fps, 16-19% → 4.3-4.9% at 90, 3.2-3.6% → 1.5-1.6% at 60 (simulated 13% → 0.12%);
      PiSP CPU 0.30 → 0.48 ms/frame at 120 fps (algorithms on every frame while it flickers). (14a52e1, 8dd8b8d)
- [x] AE locks at its limits in scenes beyond its reach (`AE_STATE` converged, as libcamera). (8f103d9)
- [x] Software ISP (`styx-softisp`): fp16 NEON path, cached capture read in place, 15 Hz stats
      when settled — 8.8–9.7 % of a core at 30 fps on one A76 core (was 47 %), bit-exact integer
      path elsewhere; x86 tone curve as fixed-point quadratics (`Arithmetic::IntPolyTone`, within
      a code; RGB24 frame 1.59 → 1.11 ms on Zen 3) and an exact AVX2 table. (66a5b1a, 5b9fde3, c7cc00b)
- [x] Built-in OV9782 description and tuning; Styx tuning search path (`STYX_TUNING_PATH`, …). (c427568)
- [x] Regions of interest without a PiSP: the software ISP processes only the region (bit-exact
      against the whole frame's crop) and bins the overview, statistics still of the whole frame
      (CM5 1280x800: 3.2 → 1.3 ms CPU per frame for a 320x200 region + overview, AE/AWB
      unchanged); libcamera ROI plans crop per output with `rpi::ScalerCrops` (2-3 frames late). (41303b9, 48b2ff0)
- [x] Several regions per client in the planner and the camera service: the PiSP (59bcbc7) and
      the software ISP (every region in the frame's pass).

### Autofocus (simulation only)
- [x] AF in Rust (`styx-algo`, from Raspberry Pi's `af.cpp`): PDAF loop, coarse + fine contrast
      scans with parabola fits, continuous mode with scene-change retriggering, windows with
      weights, manual / auto / continuous, `rpi.af` tuning import; Styx changes: frame-exact
      scan steps on lens reports, contrast relative to level, noise-aware peak tests, failed
      scans back to hyperfocal, backlash-aware approach. Simulated (`tests/it/sim_af.rs`): one-shot
      9 frames (48 with libcamera's frame counting), within 0.02 D; continuous refocus after a
      depth step in 20 frames with no hunting; PDAF 2 frames against 10; low light and blank
      walls fail cleanly (algorithms.md, "AF: autofocus"). (118edb1, b4687ae)
- [x] Lenses as data (`styx-sensor::lens`): kernel lens drivers found by the sensor's ancillary
      link (`FOCUS_ABSOLUTE`), VCMs on I²C (DW9714, DW9807/DW9817, AK7375, custom formats),
      move-time model, frame-exact `LensSchedule`, `FrameControls::lens`; IMX708 PDAF decoding
      from embedded data; software ISP focus statistics; PiSP CDAF noise from the noise profile;
      Styx controls `AF_MODE`, `AF_TRIGGER`, `AF_STATE`, `LENS_POSITION`, `AF_WINDOWS`,
      `AF_METERING`, `AF_RANGE`, `AF_SPEED`. (705dfc6)

### Ecosystem
- [x] GStreamer `styxsrc` + device provider (`crates/gst-styx`). (b0f609b)
- [x] PipeWire camera node daemon (`crates/pipewire-styx`). (788684b)
- [x] Frame socket with leases (`styx::ipc::FrameSocket`, frame-lease wire format). (a926bb0)
- [x] Public frame-lease codec (`styx_core::lease_codec`: message, descriptor order, limits,
      encode/decode, `styx-frame-lease+unix://` endpoint records) and the frame socket's hold and
      flow-control rules documented and tested (`docs/frame-server.md`). (225c0a5)
- [x] Examples for every task (listing, planned capture, async, controls and metadata, shared
      captures, other processes, raw frames and recordings, hotplug, adding a camera), run on the
      CM5; [docs/comparison.md](docs/comparison.md) against libcamera, V4L2 and GStreamer, with a
      fresh side-by-side run (open → first frame 34 vs 101 ms, CPU 1.1 vs 2.7 %, PSS 25.6 vs
      30 MiB). (57b12e2)
- [x] Processed captures take AE/AWB controls (AE on/off, fixed exposure or gain, EV, AWB
      on/off, colour temperature, red/blue gains) and report AWB's colour temperature. (7f44505)
- [x] Built-in per-camera metrics, cheap enough to leave on (262 ns per frame on the CM5;
      PiSP path CPU unchanged within measurement): frames, measured vs configured fps, drops by
      cause (sensor gaps, queue overflow, corrupt, ISP skips), sensor-to-delivery/receive
      latency, ISP and worker CPU time, 3A state, restarts, buffers held and hold times, each
      consumer of a shared capture and each camera service client; `camera_metrics()`,
      `styx::metrics::snapshot()`, Prometheus text (+ `metrics-http`), serde, the camera
      service's metrics request, `metrics_top`. Checked against the consumers' own measurement
      on the CM5. See [docs/metrics.md](docs/metrics.md). (0b1a756)

### Portability
- [x] `styx-core-rs` (formats, plane layouts, frame metadata, requirements, controls, SIMD
      kernels), `styx-algo` (all 3A and AF, tuning from TOML / Raspberry Pi JSON strings, the
      simulator), `styx-softisp` (kernels, `SoftIsp` on the calling thread), `styx-sensor`
      (descriptions, timing, gains, scheduler, embedded data, lenses, the driver over a
      `RegisterBus`), `styx-pisp` (config builders, tiling, statistics), `styx-dng` build as
      `no_std` + `alloc` without their default `std` feature; Linux builds unchanged (softisp
      benches within noise). Built for Cortex-M33, RISC-V and wasm32 in CI
      (`scripts/check-nostd.sh`); `examples/nostd-smoke` runs AE/AWB and the ISP without std
      as a host test. See [portability.md](docs/portability.md). (94b1f28, f1861e0)

### Native stack and decisions
- [x] CM5 dev box boots the bridge overlay (`camera-mode.sh native|libcamera|status`). (e7ec24d)
- [x] Default PiSP output buffer count: 6 (two frame-socket consumers holding 500 ms: 17 fps
      with 4, 30 with 6; +3.1 MB CMA for NV12 1280x800), plus the planner's extra buffers
      within half the free CMA (a slow shared consumer no longer slows the others). (d429a9a)
- [x] Black level at high gain: not a black level difference. Zero-exposure levels through the
      bridge and through the `ov9282` driver agree within 0.1 code at 1-15.5× (BLC registers the
      same); the 2.4 codes were most likely the 50 Hz lamp. To close it against libcamera
      itself: its raw at a 1-line exposure and 8× (pipeline.md, "Quality vs libcamera"). (921b0d3)

### Image quality and calibration
- [x] Camera calibration without libcamera's `ctt`: `styx-tune` (`crates/tune`, `tools/styx-tune`)
      calibrates black level (per channel and gain; hot pixels), lens shading per colour
      temperature, the AWB curve, colour matrices (chart found automatically or from corners),
      the noise profile, lux and GEQ from Styx MCAP / raw recordings or DNGs, writes Styx TOML
      and Raspberry Pi JSON (every libcamera tuning round-trips), and records the shots through
      Styx (`styx-tune capture`). Recovers a synthetic sensor's parameters (black within 0.02
      codes, shading tables 0.1%, CCM coefficients 0.011, noise 0.5%). See
      [docs/tuning.md](docs/tuning.md). (8234f6a, eece8a9)

### Performance
- [x] PiSP: 0.12 ms/frame was the `pispbe` driver writing its whole config per job with a
      barrier per word. Patched driver (`kernel-modules/pispbe`: relaxed MMIO, only changed
      words, cached config copy): 117 → 8 µs per job, Styx API 0.35 → 0.23-0.26 ms/frame at
      30 fps (0.25 → 0.13-0.15 at 120), latency −0.11 ms, outputs bit-identical, libcamera
      unaffected. Installed on the dev box as an override of `pisp_be`. (5c57f59)
- [x] PipeWire node copies each frame once: the node now allocates the PipeWire pool (memfds, and
      dma-bufs of them via `udmabuf` for DMA_DRM consumers) and the camera captures into it
      (`CaptureBuffers`: V4L2 DMABUF import, virtual camera) when frames pass through unchanged;
      a frame is held until the consumer returns its buffer. Copy otherwise (docs/ecosystem.md). (1921113)

### Platforms
- [x] `no_std` phase 2, steps 1-3 ([portability-design.md](docs/portability-design.md)):
      `styx-hal` (camera hardware traits, no `alloc`; embedded-hal 1.0 / embedded-hal-async 1.0
      as the bus vocabulary), the sensor driver over any embedded-hal I²C/SPI bus, blocking and
      async (`AsyncSensorDriver`), the `[bus]` section, descriptions compiled at build time
      (postcard), zero allocations per frame in the schedule and the sensor frame path
      (counting-allocator test); CM5 unchanged (first frame 34.4 vs 34.25 ms median, landing and
      register read-back identical). (413408a, c12b597)
- [x] `no_std` phase 2, steps 4-6 (`native/runtime`): `styx-runtime` (the sensor side,
      `Camera<P>`, frame stream and buffer leases, the sensor service; `no_std` + `alloc`),
      `styx-native` as its Linux platform (`V4l2Receiver` with the rp1-cfe handshake inside),
      the processing loop written once over `FrameIsp`/`InlineIsp` in a `no_std` pipeline core;
      CM5 unchanged (same syscalls and allocations per frame, CPU and latency within noise). (b32ebd0, 5dcba4f, 2bb9b57)
- [x] `no_std` phase 2, steps 7-8 (`work/runtime-78`): still and bracket decisions in the
      pipeline core (`still_runner`), metrics counters in the runtime (`styx_runtime::metrics`,
      `portable-atomic`), `styx` keeping the mechanics; `examples/nostd-camera`: `Camera` on a
      mock platform with the software loop, 3A, a bracket and the counters, built for the
      bare-metal targets, run on the host without `std` and bit for bit the same as over the
      `std` builds. CM5: bracket landed on consecutive frames, metrics overhead 270 ns before
      and after, CPU per frame within noise. (1a84509, f03123e)
- [x] `styx-core` frame path without `std` (`work/core-nostd`): `FrameLease`, `BufferPool`,
      shared views, `MemoryRegion` (static / DMA memory with cache and release hooks), bounded
      queues (non-blocking, waker-based async), transforms and metrics `no_std` + `alloc` (spin
      or critical-section locks; parking_lot, memfd / dma-buf, blocking waits stay with `std`);
      one metrics module (`styx_core::metrics`, the runtime re-exports it); runtime frames hand
      on as `FrameLease`s (`Frame::into_lease`, the native capture's path on Linux too);
      `examples/nostd-camera` hands its raw frames out as `FrameLease`s through a core queue.
      Linux unchanged (host benches, CM5 syscalls/allocations/CPU/latency: portability.md). (f8e2d23)
- [x] `styx-core` gaps Eidos reported (`work/core-gaps`): writable frames over caller-provided
      memory (`MemoryRegion::from_raw_mut`, `begin_cpu_write` / `end_cpu_write` hooks,
      `FrameLease::plane_data_mut`), the float shim public (`styx_core::math::Float`, used by
      `styx-pipeline` and `styx-softisp`), every atomic, the fences and `Weak` in
      `styx_core::sync`. (738af4a)
- [x] One `no_std` float shim (09e90d1): `styx-math`'s `Float`, used by `styx-algo`, `styx-dng`,
      `styx-pisp`, `styx-sensor` and re-exported by `styx-core`.
- [x] MCU footprint (`work/mcu-size`, [docs/mcu.md](docs/mcu.md)): `examples/mcu-footprint`
      links four firmware images (A: frames and a queue; B: + sensor driver and `Camera`; C:
      + AE/AWB without an ISP; D: + software ISP, full 3A, stills, metrics) for Cortex-M7/M4F
      and Cortex-M0+, `scripts/mcu-size.sh` sizes them (CI: size limits), a host test gives
      their heap. Cortex-M7, `opt-level = "z"`: A 11.5 KB, B 116 -> 89 KB, C 191 -> 128 KB,
      D 298 -> 205 KB of flash; the whole stack builds for the M0+ (A-D 11-227 KB). Cut:
      algorithms and TOML behind features (`af`, `alsc`, `awb-bayes`, `denoise`, `flicker`,
      `lux`, `toml`; all on with `std`), `from_compiled` descriptions, softisp `fp16` /
      `poly-tone`, sensor `short-history`, one shared sort, no float or `{:?}` formatting
      on firmware paths. (46c9a30, 7dff940, dff68a4, bc147c7)
- [x] Generic hardware on Lemnos 2.0 (`native/lemnos-switch`): register maps, `RegWrite`,
      error kinds (`lemnos-hal`), i2c-dev, GPIO and uevents (`lemnos-linux`), VCM drivers
      (`lemnos-drivers-vcm`), clock outputs; Styx's copies deleted (portability-design.md
      "Moved to Lemnos"). Lemnos fix on the way: i2c-dev transfers without allocation. (e30e3c7, 6d1d4a0)
- [x] Lemnos switch-over finished (`work/lemnos-switch`): `styx_sensor::RegisterBus` is
      `lemnos_hal::RegisterBus` (async: `lemnos_hal::asynch::RegisterBus`); the driver runs
      over `DriverBus` (that plus a kernel driver's V4L2 controls); `styx-native`'s and the
      spike's `regbus.rs` deleted (`I2cRegisters<I2cBus>`, `BridgePins` in `sensor_bus.rs`). (d1d4a43)
- [x] Lemnos gaps found by the switch-over closed by Lemnos `dev` 572e25d (`work/lemnos-572`):
      `styx_hal::Blocking` is `lemnos_hal::asynch::Blocking`; `styx_hal::mock::MockI2c` is
      `lemnos_hal::mock::MockI2c`; `styx_sensor::{VcmChip, VcmFormat}` are
      `lemnos_drivers_vcm::{VcmChip, OwnedVcmFormat}` (serde; descriptions and their compiled
      form unchanged, `crates/sensor/tests/it/lens_format.rs`). (96dd205)
- [x] `no_std` on targets without pointer-sized atomics (Cortex-M0, RISC-V without `a`):
      the whole stack (`styx-core` with `critical-section`: atomics, `Arc`, locks and now the
      queues through critical sections; the sensor driver, the runtime and the fp16 tables on
      `styx-core`'s `Arc`); `examples/mcu-footprint` links all four configurations for
      `thumbv6m-none-eabi`. (ea3b558, 120061a)
- [x] GPU ISP path where Vulkan exists (`styx-gpuisp`, optional): the software ISP's pipeline
      as Vulkan compute shaders (ash, Vulkan loaded at run time), bit-exact with the integer
      arithmetic (pictures and statistics, RADV and llvmpipe), capture dma-bufs imported and
      outputs exportable, one submission per frame; `SoftLoop::use_gpu`, Styx feature
      `gpu-isp` with a planner cost. RX 6800 XT: 0.47 ms CPU per 1280x800 NV12 frame against
      1.77 ms for the software ISP on the same host, GPU 0.21 ms. (869416a)
- [x] Userspace UVC (optional): `styx-uvc` over usbfs, `BackendKind::Uvc` (feature `uvc`;
      `uvcvideo` stays the default where it has the camera). C270 on the CM5: YUYV and MJPEG
      at 30 fps, controls, replug, hotplug; PTS timestamps with no jitter (uvcvideo: 1.9 ms
      sd); ~0.5-1% of a core more CPU. See [docs/uvc.md](docs/uvc.md). (2069961)

### Stills and DNG
- [x] Still capture on a running capture (`capture_still` / `request_still` on `CaptureHandle`
      and `Frames`): JPEG/NV12/RGB/raw, DNG, fixed exposure, EV brackets on the frames the
      control schedule names, AE settle. CM5: preview 30.00 fps with 0 gaps while taking
      stills; still 95-134 ms request → ready, bracket of 3 on consecutive frames in ~340 ms
      (docs/stills-and-dng.md). (9c9e993, 2c149a5)
- [x] `styx-dng`: DNG 1.4 writer (calibration from the tuning, GainMap lens shading, EXIF,
      preview) and reader (camera DNGs: lossless JPEG tiles, packed samples); LibRaw renders
      Styx's DNGs. (17b6394)

### Metrics and fuzzing
- [x] AF in the metrics: state, mode, lens position (dioptres), lens settled, scans started
      (unit-tested; no camera with a lens on the dev box yet). (2a5b7bc)
- [x] cargo-fuzz targets for every parser of untrusted bytes but the netcam multipart parser (below) (UVC descriptors and streams, sensor
      descriptions, embedded data and kernel-driver reports, tuning files, PiSP statistics,
      kernel messages, raw and MJPEG decoders, imported frame layouts, frame socket and camera
      service messages and requests, recordings, raw recordings), `scripts/fuzz.sh`, nightly CI
      smoke run; see [docs/fuzzing.md](docs/fuzzing.md). Found and fixed: out-of-bounds reads in
      odd-width UYVY/NV12 decoding, SIGBUS on a short memfd from another process, unchecked
      imported descriptors, allocations sized from headers, overflow panics. (1bf166d)

### Known issues fixed
- [x] 120 fps AE sometimes chases 100 Hz flicker (kernel-driver path); add anti-flicker. (30b2826)
- [x] Flicker stays in the frames at exposures shorter than a period: deflicker (above). (14a52e1)
- [x] `AE_STATE` never reported converged when AE's target was out of reach; AE now reports
      converged once pinned at its limits (`AeStatus::at_limit`), as libcamera does. (8f103d9)
- [x] OV9782 raw8 (`BA81`) through the bridge: frames reported 342.9 ms × 0.5 at 30 fps and were
      far darker than RAW10. The raw8 embedded line carries each 10-bit word's top 8 bits
      (decoded as RAW10 it said VTS 4, and the schedule then limited exposure to that 4-line
      frame), and the raw8 PLL runs at 192 MHz, not the driver's 200 MHz (30 fps ran at 28.8).
      raw8 now has `pixel_rate = 192 MHz` and `embedded_data = false` (predicted values):
      30 / 120 / 5 fps exact, levels equal RAW10's top 8 bits at the same exposure and gain. (d417512)
- [x] Shared captures on the PiSP: a slow consumer with a deep queue paced the camera for every
      consumer. Back end buffers now default to 6 plus the planner's `capture_extra_buffers`
      (within half the free CMA); a consumer sleeping 200 ms gets 5 fps, the other keeps 30. (d429a9a)
- [x] MCAP recordings drop a native frame's exposure and gains (`NativeFrameMeta`): MCAP format
      2 and `.styxrec` 2 record exposure, gains, frame duration and length, verified, error;
      version 1 files still read. AE state, colour temperature and lux are controls, not
      per-frame metadata, so not recorded (add them to `NativeFrameMeta` first). (9efd60c)
- [x] A plan that asks no frame rate gets the mode's fastest (camera service clients: 260 fps
      at 640x400): modes with a rate range now run at `planner::DEFAULT_FPS` (30, within the
      mode's range) without `min_fps`, in single and shared plans and plain native captures. (a658866)

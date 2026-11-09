# Styx native stack: progress and TODO

Styx as a stand-alone camera stack: USB and CSI cameras with nothing but the Linux kernel
underneath. No libcamera, no `v4l` crate, no per-sensor kernel driver. Design and device rules:
[docs/native-stack/README.md](docs/native-stack/README.md).

No patches on other people's code; our own kernel modules are fine. The stock V4L2 path stays
working in Styx everywhere: SystemCore and other kernels we don't control depend on it. The
Styx sensor bridge is gone; the kernel module is Lemnos's, with the drivers in Rust in userspace
([The leap](#the-leap-started-2026-11-06), item 4). The OV9782 driver (upstream `ov9282` plus our
patch) becomes one Lemnos-owned sensor module with its mode and register data, and Styx's
`crates/sensor` copy of that data goes (user decision, 2026-11-07).

Audited against the code at 475e07a (2026-11-01); re-checked at 673ade94 (2026-11-07) for device
access, the acceleration counts, K230 wiring and the retired crates. Numbers are from the CM5 / Raze with the OV9782
at 1280x800 unless stated. Every open item has *what* (its bold title), *why*, *done when*,
*blocked by* and *numbers*; "written, untested on board" means the code exists and only unit
tests or QEMU have run it. Done items move to [Done](#done) with their commit.

The user alone decides when a release is cut and tagged, and what blocks one. Versions stay on
the dev scheme (`2.0.1-dev` now; only the patch moves); the next release will be 3.0. Board work
gets tested on boards before anything ships, unless the user says otherwise. **Held** and
**paused** items wait for the user. Accelerators are chosen through plain `StyxConfig` knobs per
device, with a CPU fallback for each; CPU SIMD stays detected, as it changes no output.

## Now

The clear-out before the leap (coordinator, 2026-11-06, the user's go). Land what is in flight,
close or drop stale items with a reason, park board-only work as written, untested on board.

- [ ] **Land the stack** (gate, push, report): the NV12 pyramid fix (83e11b2), the frame socket
      removal (ae67432), served CPU levels in memfd (299afd1), the mapped pool (3eb841b), CPU
      constants (eb7c610), 4 front end buffers (9dad856), perf-smoke scale (5e35ba7), and the
      joint lock: Daedalus 04d747d, Lemnos 80d5d1e.
- [ ] **Raw buffers only while a raw consumer is attached** (coordinator, 2026-11-06). *Why:* 20 ×
      2 MB raw buffers with `sensor_raw` available (peripherals RSS 69 MB, 33 MB without). *Done
      when:* the raw lease's buffers exist only while a raw consumer is attached and are freed when
      it leaves, with at most one capture restart each way, its cost stated. *Written:* a raw
      consumer joining turns `SENSOR_RAW` on in the running capture: the PiSP adds 4 front end
      buffers (`add_image_buffers`) and gives them to the back end (`add_inputs`), no restart;
      the software ISP just attaches them. They stay until the next restart (the driver frees
      buffers only with the stream off). HeliOS attaches its raw client only while recording
      (99ea403e). Host test: a raw client on a simulated camera, 0 restarts. PiSP untested on
      board.
- [ ] **One capture start on an fps change** (the triple start). *Why:* clients reconnecting one at
      a time each shrink and regrow the plan. Written: a client that leaves keeps its setup for
      `LEAVE_GRACE` (250 ms) before the others' plan shrinks. *Done when:* one start per change
      on the Raze.
- [ ] **Compressed raw on the board** (written, untested on board; needs Lemnos a461e81, after the
      joint lock's 80d5d1e, so it lands with the next Lemnos round). *Done when:* Raze #1
      streams with `STYX_PISP_RAW_COMPRESSED=1`, image compared with uncompressed and memory stated.
- [ ] **A same-request join with every spare consumer in use restarts the capture** (HeliOS
      alpha, make-before-break redeploys). *Done when:* such a join shares the existing consumer's
      frames instead of growing capacity.
- [ ] **YUYV no-frames fallback: the bench check** (no longer paused: only anti-flicker is). *Why:*
      a UVC camera that negotiates YUYV and delivers nothing hung a client. The fallback and the
      cause in the error are in (302b1f6, 9cb7009: `planner/stalled.rs`; unit tests in
      `planner/usb_tests.rs`). *Done when:* the C270 on the CM5 in a YUYV mode it cannot sustain
      falls back to MJPEG and the error names the cause. *Blocked by:* the C270 plugged in.

## The leap (started 2026-11-06)

The user's go, relayed by the coordinator on 2026-11-06. One small Lemnos kernel module with Rust
userspace drivers; every shared library in static and shared forms (Rust core, a small versioned C
surface, a static Rust client wrapper, never both in one process); `lemnos-abi` holds the shared
handles, status, slabs and fences. Static fat LTO stays the default; shared mode is for consumers
outside the image, and per-frame hot loops stay inside one library. Styx's part: items 1, 2, 4
and 6 below, the stock V4L2 fallback kept.

1. [ ] **Consumer C surface and static Rust wrapper on `lemnos-abi`,** with a recycled handle slab,
      timed on the CM5. *Written:* `crates/capi` (`styx-capi`), `include/styx.h` 1.0 on
      `lemnos-abi` 1.0: handshake, cameras (probe, names, a virtual camera), streams (open, next,
      close), frames (`LemnosBufferDesc` dma-bufs, host planes, release) and controls; staticlib,
      `libstyx_capi.so.1` and the Rust wrapper through the same calls. Host tests: the wrapper on
      a virtual camera, stale handles refused, a C program linked static and shared. Not yet:
      fences, the CM5 timing. ISP, 3A and sensor traits stay internal. *Why:* consumers link Styx through
      one boundary instead of its Rust types. *Done when:* a consumer takes frames through the C
      surface and the wrapper, with the cost per frame measured on the CM5. *Numbers:* audit
      estimate under 1 µs a frame; `FrameLease` 552 B, `FrameMeta` 328 B, `NativeFrameMeta` 88 B;
      duplicate statics are the risk in shared mode.
2. [ ] **Frames on Lemnos buffers.** *Why:* one buffer type under the stack. *Done when:* capture
      leases sit on Lemnos's buffers with no copy added and 0 allocations in steady state.
3. [x] **Port onto Lemnos.** The generic parts of `crates/kernel` move there; Styx keeps the camera
      pipeline and tuning. *Why:* hardware blocks live in one place. *Done:* Styx on Lemnos rev
      4dceae2; `crates/kernel` and `crates/pisp` deleted. `lemnos-v4l2` (v4l2/subdev/media/event),
      `lemnos-linux` `v4l2-m2m` under `styx_codec::m2m` (the session wrapper Styx's codecs use),
      `lemnos-drivers-pisp`, `lemnos-drivers-rkisp` + `lemnos-rkisp-sys` and
      `lemnos-drivers-k230isp` under `styx-rkisp`/`styx-k230isp` (params, stats, `AnyParams`,
      `IspBuffers` and the K230 builder stay here), `lemnos-sensor-bridge` for `bus`,
      `lemnos-drivers-rga` for `daedalus-rga`. `styx_native::dma` keeps Styx's tail pad
      (Lemnos's `allocate_importable`, 952e1c7; CMA capped by `CmaTotal` since cf3a4ac) and memfd stand-in on `lemnos_linux::dma`; `sync_stats` is Lemnos's.
      `kernel-modules/styx-sensor-bridge` is deleted; its successor is Lemnos's
      `kernel-modules/lemnos-sensor-bridge` (2676ccd). Images ship neither.
      Raze #1, 2026-11-06, 120 fps sliced (line 600), A/B against 9d5a3bf on the stock modules:
      frame-out p50 7.975 / 7.975 ms, p99 9.294 / 9.298 ms; 1199/1200 sliced, 0 early fires;
      149/149 verified stripes byte-identical; 0 steady-state allocations in the pipeline (the
      tool's own record Vecs grow 15 times in 960 frames, the same on both).
4. [ ] **Move onto the Lemnos kernel module and Rust userspace drivers.** An own receiver replaces
      the stock `rp1-cfe`, the sensor runs over i2c-dev, the PiSP moves onto it, and the bridge and
      the OV9782 patches retire. The stock V4L2 path stays as the fallback. *Why:* one small module
      of ours (claim, DMA, dma-buf, interrupts, registers) instead of other people's drivers. *Done when:* the Raze runs on it and matches the targets in Done ("Now list of 2026-11-03"), and the
      stock path still passes. *Numbers:* 552 rows by about 6.15 ms, the whole frame by 8.1-8.5 ms,
      a line signal by 5.74 ms.
5. [ ] **`Submit::Own` in gpuisp: agree device ownership** per gpu-seam.md. *Why:* in a `daedalus`
      build a failed `GpuContext::shared()` falls back to a device of its own
      (`crates/styx/src/gpu_isp.rs`), so the process holds two Vulkan devices, against one device
      and queue per process. The no-Daedalus build and `STYX_GPUISP_DEVICE` use the same path on
      purpose (`crates/gpuisp/src/context.rs`). *Done when:* who owns the device is agreed, and the
      fallback either goes or says why it stays. *Numbers:* none; gpu-seam.md is not in Styx,
      HeliOS, Daedalus, Eidos or Lemnos's tracked files, so it has to be found first.
6. [ ] **Shared and static build forms.** Static and shared modes, core, surface and wrapper per
      dependency. Gaia's LTO test informs how shared gets built. *Why:* the same code ships either
      way. *Done when:* both forms build and run on the Raze with their cost measured.
7. [x] **A `FrameLease` from Daedalus's stable frame handle** (`styx_core::daedalus::from_shared_frame`,
      user's go 2026-11-05: Styx plus Daedalus's existing types). A `SharedFrame` becomes an
      `Arc<FrameLease>` over a pooled backing holding one retain of it: planes through the
      frame's `plane_data` in place, bracketed reads passed through, dma-bufs borrowed (the lease
      keeps the frame, so its fds, alive: no dup), residency, CPU access, colour, timestamp and
      sequence from the frame, read-only. The same frame gives the same `Arc` while it lives
      (`Weak`s keyed by the frame's data pointer); a Styx frame sent in with
      `shared_frame_payload` comes back as the very `Arc` it went in as, with no pointer casts.
      Chosen over Daedalus's converter hook: nothing needed on Daedalus's side, and a hook can
      call this if one comes. Not carried: capture metadata (exposure, gains), which a lease
      keeps per backend. Host tests only (`daedalus/shared_frame_tests.rs`: in place, same
      `Arc`, one release on each of the three drop paths, no CPU access left open); Eidos has
      not run it. *Numbers:* one allocation per new frame, the `Arc<FrameLease>` (a live `Weak`
      keeps its allocation, so it cannot be pooled), 0 for a frame already a lease
      (`tests/shared_frame_allocations.rs`).

## Alpha (Raze)

The next release is an alpha for the Raze (CM5, OV9782, PiSP) only; the other boards follow
right after it. These items come first. Paused and held items stay paused or held until the
user lifts them.

- [x] **GPU ISP on the Pi 5: timing measured and profiled** (crates/gpuisp/README.md, "Raspberry
      Pi 5", and docs/native-stack/pipeline.md). 2026-11-06, Raze, `native-pipeline gpu-bench` on a
      RAW10 recording, 1280x800 RGB24: 28.9 ms of GPU a frame (27.7 with the tile fill unrolled,
      kept), 2.65 ms on one A76 core in fp16. By cutting parts out: other passes 2.7, skeleton 5.5,
      tile fill 10.5, tone and colour 5.7, demosaic 3.2, stores 0. Bound by memory latency:
      specialising (4 threads), the parameters as a uniform block and RAW10 unpacked from words
      gave 27.4, 27.7 and 31.0. Off by default on V3D stays (`STYX_GPU_ISP=1` takes it); a faster
      V3D frame needs a different shader shape, not tuning. Live 2026-11-02 numbers: 28.3 fps,
      latency 133 ms, CPU 1.97 ms a frame, against the CPU software ISP's 59.98 fps, 12.3 ms, 9.5
      ms CPU.
- [x] **`native-pipeline regcheck`: `0x0101` and `0x1000` read back 0.** Description fixed
      (docs/portability-design.md, Gate B): 91 of 93 as in the baseline. Both registers come from
      the driver's init table, are in no OV9281/OV9282 datasheet, and read 0 after any write on the
      CM5 (bursts and one transfer each, the baseline too) while the sensor streams normally, so
      they are not called write-only without a source.
- [x] **Still JPEG encoding numbers.** Re-measured with turbojpeg on Raze #1, 2026-11-06
      (`still_capture DIR 30 native`, static aarch64 musl, 9253cbc): the still lands now (the
      `settle(true)` fix: wait half the timeout, then take AE's exposure). Reprocess + JPEG + DNG on
      the still thread 25.3-27.5 ms over 4 shots, against 44-49 ms with the `image` crate; request →
      ready 100.1 ms, bracket 300.7 ms, all three on their frames, preview 30.00 fps with 0 gaps
      (docs/stills-and-dng.md).
- [ ] **Anti-flicker verified at 120 fps** (paused: the user paused bench tests). *Why:* AE chased
      100 Hz flicker at 120 fps under mains lamps. *Done when:* `flicker_check` on the CM5/Raze
      under a 50 Hz lamp at 120 fps: AE locks and the output frame-to-frame spread stays under
      5%, with flicker avoidance and deflicker on (the defaults). *Blocked by:* the pause and a
      mains lamp at the bench in a Raze slot. *Numbers:* room lamp at 120 fps, 15-18% → 3.4-4.7%
      spread; not yet the checked run this item asks for.
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
- [ ] **K230D: VPU encode (H.264/H.265/JPEG)** (written, untested on board). `V4l2M2mEncoder`
      drives any stateful V4L2 encoder, `mvx` included, and the registry offers it for NV12, YU12
      and YUYV when a device here produces H.264, HEVC or JPEG. Frames over dma-bufs at the
      device's layout (capture buffers) are handed to it by descriptor (`M2mSession::
      open_importing`), no copy; others are copied. *Why:* preview and recording without the
      CPU. *Done when:* `/dev/video0` (`mvx`) encodes 1280x800 30 fps from the capture's dma-bufs
      with no copy (the session imports), and the CPU fallback is used without it. Numbers: the
      import path passes against `vim2m` on the PC (`dmabuf_frames_are_read_in_place_or_copied`,
      `vim2m_reads_dmabufs_in_place`). *Blocked by:* a K230D board.
- [ ] **K230D: 2D unit scaling** (written, untested on board). `V4l2M2mConverter` drives any V4L2
      M2M scaler or converter, `nonai-2d` included, with the same dma-buf import. *Why:* preview
      frames and levels without the CPU. *Done when:* `/dev/video5` (`nonai-2d`) scales the
      capture's NV12 through the M2M backend within a stated tolerance of the box filter.
      *Blocked by:* a K230D board. The planner's scaling stage (`planner/scaling.rs`) picks the
      converter for a request's frame size; pyramid levels and the preview still scale on the CPU.
- [ ] **RK3588S: ISP30 bring-up** (written, untested on board). *Why:* the Rockchip HVS boards. *Done
      when:* the OV9782 through rkcif and the ISP streams NV12 at 120 fps with 3A converged; entity
      names (`media-ctl -p`) and the fixed-point constants (gains, CCM, LSC, statistics bit
      depths) confirmed. *Blocked by:* an RK3588S board on the vendor 6.1 BSP.
- [ ] **Rockchip: rkcif graph per chip** (`IspDevice` sets up one graph, written, untested on board).
      *Why:* capture feeds the ISP. *Done when:* each chip's (RK3588S, RK3576, RK3566, RK3562,
      RV1106) sensor-to-ISP media graph is set up by Styx and streams. *Blocked by:* boards.
- [ ] **Rockchip: the ISP's self path for pyramid levels and preview: written, untested on
      board.** `IspSetup::self_path` links the ISP's source pad to `rkisp_selfpath` and keeps a
      second NV12 queue; `RkispPipeline` dequeues its frame with each main frame and the worker
      attaches it as a `CompanionKind::Pyramid` companion (`native_pyramid_level`, copied into a
      pooled buffer; the main frame stays zero-copy). *Why:* a second scaled output costs
      nothing. *Done when:* on a board the media core takes the second enabled link from the
      source pad, level 1 (and deeper levels from it) arrives within a stated tolerance of the
      box filter, and the companion is leased without the copy. *Blocked by:* a Rockchip board
      (RK3588S, RK3566 or a Flame).
- [ ] **RK3566: rkisp ISP21** (`styx-rkisp` parameters and statistics, 0dad505, written, untested on
      board). *Why:* user decision, both low-cost chips. *Done when:* the chip streams the OV9782
      with 3A converged. The structures, builder and parser have tests; the fixed-point constants
      and vertex layout are the headers' silence, to confirm on the chip. `styx_pipeline::rkisp`
      picks the generation from the SoC (`Isp::detect`, written, untested on board).
      *Blocked by:* an RK3566 board.
- [ ] **RK3562: rkisp ISP32-lite** (parameters and statistics, e9adfc0, written, untested on board).
      *Why:* user decision, both low-cost chips. *Done when:* as RK3566. The pipeline selects it
      from the SoC (`Isp::detect`: `rockchip,rk3562`; written, untested on board). *Blocked by:*
      an RK3562 board.
- [ ] **RK3576: rkisp, its generation.** Not written: the docs list it as a fallback, its generation
      is unconfirmed. *Why:* a supported chip. *Done when:* the generation is confirmed from the
      BSP's rkisp hardware table, then as RK3566. *Blocked by:* an RK3576 board and BSP.
- [ ] **RV1126B: rkisp ISP35.** Not written. *Why:* Luckfox Aura. Its cameras run the software ISP
      until then (`has_rkisp`, written, untested on board). *Done when:* as RK3566. *Blocked by:*
      the ISP35 layouts and detection in Lemnos (see docs/boards.md), then an RV1126B board.
- [ ] **A733: capture, ISP, encode, scaler.** Multi-planar capture for `sunxi-vin` is written, untested
      on board, and H.264 through CedarC (`codec-cedar`), same; the ISP and scaler are not. docs/boards.md
      has what each needs.
      *Why:* Cubie A7A, A7S, A7Z. *Done when:* a camera streams through `sunxi-vin` with 3A
      converged and encodes without the CPU. *Blocked by:* `VIDIOC_S_INPUT` in Lemnos's
      `VideoDevice`, then an A733 board.
- [ ] **Rockchip: RGA scale, crop and convert** (RGA3 on RK3588S, RGA2 on the others). *Why:* deeper
      pyramid levels and preview frames without the CPU. RGA moves to Lemnos on V4L2 M2M with
      [The leap](#the-leap-started-2026-11-06) item 3, no librga; until then the binding is
      Daedalus's `daedalus-rga`. `V4l2M2mConverter` is not for RGA.
      Written, untested on board: `styx_codec::RgaConverter` (feature `codec-rga`), dma-buf frames
      read in place, host frames through dma-heap buffers, scale/crop/convert tests against vim2m
      (identity and channel swap exact). Format conversion is hooked up: `CodecKind::Converter`, the
      registry offers RGA conversions between NV12, NV21, YU12, RG24, BG24, BGRA and GREY when a
      `rockchip-rga` node exists, the planner takes them where it takes a raw decoder (hardware first,
      `Hardware::Off` falls back to the CPU), and encoder lookups never take them. The planner's
      scaling stage (`planner/scaling.rs`) scales to a request's frame size through RGA, or a V4L2
      M2M scaler on chips listed with one; written, untested on board, its cost an estimate. Pyramid
      levels and the preview still scale on the CPU. *Done when:* the planner scales through RGA
      within a stated tolerance on a board. *Blocked by:* a Rockchip board.
- [ ] **Rockchip: MPP / rkvenc H.264/H.265 and the JPEG encoder** (written, untested on board:
      `styx_codec::rkmpp::RkMppEncoder`, feature `codec-rkmpp`, MPP opened with `dlopen`). NV12 or
      YUYV in; the ISP's NV12 in one dma-buf is imported and encoded in place, other frames are
      copied into two dma-heap staging buffers imported once. The registry offers it when
      `librockchip_mpp` and `/dev/mpp_service` are here. Stateless V4L2 VPUs (mainline Hantro,
      rkvenc) are still not written: `V4l2M2mEncoder` is stateful only. *Why:* preview and
      recording without turbojpeg on the CPU. *Done when:* it encodes the ISP's dma-buf NV12 at
      1280x800 120 fps with the session importing (no staging copy), keyframes flagged, and the
      CPU fallback is used without it; the constants in `rkmpp/ffi.rs` (command numbers, coding
      types, RC and header modes) confirmed against the BSP's headers. Numbers: layout and
      constant tests on the PC only. *Blocked by:* a Rockchip board on the vendor BSP
      (`mpp_service`).
- [ ] **RV1106: pyramid levels from the ISP: the self path written, untested on board.** As the
      other chips (the self path item above); the ISP32's 4x4-sampled outputs
      (`isp32::nodes::MAINPATH_4X4`) are not used yet. *Why:* Eidos on Flame spends the most Styx
      time on the CPU pyramid downscale. *Done when:* the companion arrives on the Flame at
      256-byte strides (luma or NV12's Y plane), `box2_row` the fallback for deeper levels.
      *Blocked by:* a Flame board and its BSP.
- [ ] **Flame (RV1106G2): bring-up** (ISP32 parameters and statistics written, untested on board;
      pipeline selection as RK3562). *Why:* the Flame board. *Done when:* rkisp, rkcif graph and
      rkvenc run the OV9782 within the RV1106G2's memory, with the CPU fallbacks. *Blocked by:* a
      Flame board and its BSP.
## Open: every stage on every backend, and allocations

- [ ] **Every stage accelerated on every backend** ([docs/acceleration.md](docs/acceleration.md)).
      *Why:* user rule: every stage that can be accelerated is, on x86, NEON64, armv7 NEON, RVV, the
      GPU and each board's fixed-function units; losing variants stay behind knobs. *Done when:* the
      matrix has no gap cells; each leaf conformance-tested against scalar (guard bytes, odd widths)
      in the gate, on hardware or QEMU. *Blocked by:* nothing for the CPU and GPU columns; boards
      for the fixed-function rows. *Numbers:* recounted from the matrix (2026-11-07): **239 pass, 15 lib, 3
      gaps, 1 n/a of 258 cells** (43 stages). The gaps: fp16 on x86, and JPEG decode on RVV and on
      the GPU. Fixed-function: 5 pass, 7 gaps, 6 of which need a board.
- [ ] **Software ISP: AVX-512 VBMI exact tone table: correct under Intel SDE, never timed.**
      `simd/x86/tone512.rs`: both 256-entry node lookups as two `vpermi2b` and a blend, 64
      pixels at a time; dispatched where AVX-512 F, BW and VBMI are detected
      (`X86FeatureSet::avx512vbmi`, `SimdBackend::X86Avx512Vbmi`). *Why:* the fastest exact
      table on such hosts. *Done when:* its time is in PERFORMANCE.md from a real AVX-512 VBMI
      CPU. *Blocked by:* such a CPU (the dev PC is a Zen 3). CI's "AVX-512 VBMI tone leaf"
      step runs `avx512vbmi_tone_leaf` on every push and prints "ran on AVX-512 VBMI" or
      "skipped: no AVX-512 here"; record the run id and CPU here when one runs it.
      *Numbers:* correctness, 2026-11-02, `scripts/check-sde.sh` under SDE 10.13.1 `-icx`:
      `avx512vbmi_tone_leaf` ran the leaf, `lut_every_code` matched scalar on every code and
      node shape, and all styx-softisp (46 lib, 27 it, 3 alloc) and styx-core (158) tests
      passed. Beside it: colour matrix 1.1 ms and tone table 1.5 ms per frame on the A76's
      integer path; never measured on a real Cortex-A72.
- [ ] **GPU ISP: outputs through the capture, and the tables.** *Why:* `GpuIsp` exports dma-bufs
      (869416a) but the capture copies them into heap frames; the 0.2 ms table rebuild per settings
      change could move to the GPU at the cost of bit-exactness. *Done when:* the capture hands out the
      exported dma-buf with `CopySite` zero; the table variant measured behind a knob. *Blocked by:*
      nothing. *Numbers:* RX 6800 XT 0.47 ms CPU, 0.21 ms GPU per frame; tables 0.2 ms.
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

Parked until after the leap: none blocks it. Not on the freeze list. *Numbers:* none measured unless stated.

- [ ] **MCU flash: watch config C on the Cortex-M7.** *What:* C is 131,704 bytes against its
      134,000 limit (1.7% headroom; the others have 4-11%, D 5%). *Why:* the limits are guards,
      and lean is the default: the next growth in the code C links (sensor driver, algorithms,
      descriptions) should be cut, not covered by a new limit. *Done when:* C has about 5%
      headroom again from a real saving. *Blocked by:* nothing. *Numbers:* `scripts/mcu-size.sh`
      at cb3f56e; docs/mcu.md. A saving looked at and not taken: `SoftLoop::new` clones the
      `CameraConfig` (about 375 bytes of clone code in D), but `SoftLoop::info()` hands out the
      whole `SensorInfo`, so moving it into the controller would change that API.
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
- [ ] **AF on a userspace-driven sensor with an I²C VCM.** *Why:* untested combination; the DW9807 busy flag
      is not polled. *Done when:* it focuses on one. *Blocked by:* a module, and
      [The leap](#the-leap-started-2026-11-06) item 4.
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
- [ ] **A second userspace-driven sensor and a non-Pi board.** *Why:* only the OV9782 on the CM5 has run
      without a kernel sensor driver. *Done when:* one of each streams over i2c-dev. *Blocked by:*
      hardware, and [The leap](#the-leap-started-2026-11-06) item 4.
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
- [ ] **Lemnos from crates.io.** *Why:* root `Cargo.toml` and `fuzz/Cargo.toml` use the `dev` branch by git.
      *Done when:* switched to 2.0 from crates.io. *Blocked by:* Lemnos publishing it.
- [ ] **Native provider hotplug on uevents.** *Why:* `crates/native/src/provider.rs` polls sysfs every second;
      `styx::watch` already uses `lemnos_linux::uevent`. *Done when:* the provider does too. *Blocked by:*
      nothing. *Numbers:* up to 1 s latency.
- [ ] **`no_std` replays across builds.** *Why:* libm can differ from std in the last bits, and `Auto` picks by
      build. *Done when:* documented comparison runs ask for `Int`, or the results are shown equal. *Blocked
      by:* nothing. *Numbers:* identical over `nostd-camera`'s 90 frames.
- [ ] **Stills, remaining.** *Why:* V4L2 stills reconfigure; UVC still-image methods are missing; stills on
      a capture without a processed native mode leave the consumer one frame short. *Done when:* each
      is gone. *Blocked by:* a camera with still methods for UVC.
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
- [ ] **Deflicker corners.** *Why:* rolling-shutter band gains are unverified on a sensor (needs `readout` from
      its description); 30 fps with short exposures leaves ~2%; with deflicker off, a clipped lamp makes AE
      chase the beat at 120 fps (simulated). *Done when:* verified on a sensor, or accepted by the user.
      *Blocked by:* the bench pause. *Numbers:* as stated.

## Done

Numbers are as of the commit named.

### Since the last audit

- [x] **PiSP back end ops for Eidos on the Raze** (`styx_pisp::be::ops` + `device::BeImageDevice`).
      *Why:* Eidos lowers resample/downscale (pyramids), tone LUTs, 5x5 filters and demosaic + CCM
      onto the BE memory to memory on its own dma-bufs. *Done when:* the ops run for Eidos on the board, GREY on the
      NV12 route beside live frames included. *Blocked by:* nothing. *Numbers:* Raze #2, 2026-11-03, GREY 640x480, buffers from `linux,cma`: `box2`,
      `downscale`, `pyramid2` 0.40-0.42 ms, truncated (59280 of 76800 one below rounding half up,
      none further; 10680 of 19200 on level 2); `lut` 0.72 ms, max 5 from the table against
      `LutFit`'s predicted 4; `resample` 0.40-0.85 ms over single runs, within 1 of the CPU model (2979 of 76800 differ);
      `filter5x5` 0.36 ms, exact at unit gain clear of the 2-pixel border; `demosaic_ccm` 0.22 ms, four
      flat patches exact.
      4232 runs on group 1 beside live frames on group 0: 0 failed, 0 drops.
      *Closed 2026-11-06:* the ops run and are measured on the board beside live frames; using them is Eidos's item.
- [x] **Zero steady-state allocations: what isn't counted, or isn't 0.** *Why:* standing rule for
      every dependency: a count of 1-3 per frame is a finding; assert 0, or state the physical reason.
      *Done when:* every per-frame path below asserts 0, or names its reason. *Blocked by:* a device for libcamera. Raze 2026-11-02, `hop_breakdown inproc` (PiSP NV12
      1280x800 30 fps, whole process, every thread): 0.00 heap allocations per frame in steady
      state (median of 9 one-second windows), 0.04 over the whole run. *Numbers:* asserted 0 (6a6c40d): PiSP lease with and
      without a companion, UVC frame lease, GPU ISP frame from memory, PiSP processed path on a host
      (`pisp_alloc`). Also asserted 0: software ISP output lease and rkisp lease
      (`frames_allocate_nothing`, `BackingPool`); a `SoftIsp` frame at 1 and 4 threads, plain and post
      stages, RGB24/NV12/I420, and with statistics when the caller uses `recycle_stats`
      (`crates/softisp/tests/alloc.rs`), also with white balance, gain, lens shading and tone changing
      every frame (`swap_params`, Int, IntPolyTone and Half). The software ISP loop (`SoftLoop`, the
      algorithms included) on a host when its outputs are handed back (`SoftLoop::recycle`): 0.00 per
      frame over 300 frames, 176 with the algorithms and 124 settled ones without (`soft_alloc`; was
      129.83). The replay source's software ISP (`replay_isp.rs`): 0, its NV12 frames in pooled
      buffers (`a_replayed_frame_allocates_nothing_when_warm`; was a new buffer, a copy and the
      controls' clone per frame). Also asserted 0 (`crates/styx/tests/worker_alloc.rs`): the capture workers on a
      simulated OV9782 (the real `NativeCamera`, queue, sensor driver and control schedule), raw
      frames (`styx-native-capture`, its lease records now from a `BackingPool`) and the software
      ISP worker with its helpers and the 3A, 120 frames each, consumer included (were 5.60 raw and
      2.02 processed: the lease record, and per `next_blocking` the executor's waker, the timeout's
      box and the reactor's timer map node; under load, a lease record the warm-up had not made: the
      pool makes all of them up front now, 0 in 64 runs at load 39). Whole process
      on the Raze, 2026-11-03, frames 60-300 at 30 fps: UVC MJPG and YUYV 0.000 (was 1.007: the
      stream's frame list lost its capacity every frame). V4L2 MJPG and YUYV 0.000 (2026-11-05,
      `uvc_capture` counts and traces them now; were 0.033: the buffer-recycle channel was an
      unbounded `mpsc`, a block every 31 frames, and a stage's timing window grew by doubling). **Not counted:** libcamera, and the kernel's side of the queue
      on a board. Also asserted 0: the PiSP raw lease, whole and at a sliced frame's
      first slice (`a_raw_lease_allocates_nothing`), and the sliced frame's states, reused once
      their leases return (`slice_states_are_reused_once_free`; were 4 `Arc`s per frame). GPU ISP (`crates/gpuisp/tests/it/
      alloc.rs`): 0 with statistics handed back (`recycle_stats`) and white balance, gain, lens
      shading and tone changing every frame (`swap_params`, tables rebuilt in place); was a new
      `IspStats` and new tables per frame. `Input::Buffer` on Daedalus's device: 0, submitted
      through Daedalus's `RawSubmitter` (Daedalus fd18808); was 17 per frame in `submit_raw`
      and wgpu-core's submission.
      *Closed 2026-11-06:* every native per-frame path asserts 0; libcamera is off the native stack and the kernel's queue is not ours to count.
- [x] **CM5 HEVC decode check** (`hevc_v4l2request` through FFmpeg). *Why:* HEVC recordings only; Styx
      records MJPEG and raw. *Done when:* a recording decodes through `rpi-hevc-dec`. *Blocked by:*
      an Atlas image with that driver; HeliOS says when. *Numbers:* none: the driver is not in the
      image.
      *Closed 2026-11-06:* dropped: Styx records MJPEG and raw, not HEVC; the decoder is the image's question (HeliOS).
- [x] **Runtime follow-ups.** *Why:* the PiSP's external "driven" path still runs the event thread and
      `serve_sync` directly; `Receiver::poll_sync` on Linux wakes only at stop. *Done when:*
      `PispFeReceiver` is a `Receiver` (designed in docs/portability-design.md, not written). Static
      dispatch only if a profile shows it (`dyn` measured free). *Blocked by:* nothing.
      *Closed 2026-11-06:* dropped: the PiSP receiver moves onto the Lemnos module ([The leap](#the-leap-started-2026-11-06) item 4), which replaces this design.
- [x] **Reject impossible frame-length reports.** *Why:* a value from embedded data is trusted even when
      impossible (VTS 4 on an 800-line mode). *Done:* `ControlScheduler::with_frame_lengths` (set from
      the mode's timing) drops frame lengths outside the mode and exposures past the longest frame,
      counted in `rejected_reports()`.

- [x] **Row-sliced box2 (Eidos): measured, no gain, not added.** A banded
      `box_downscale_luma_in` that hands over each run of half-level rows (callback after 8, 16 or
      32 rows) to be thresholded in cache, against making the level then thresholding it. Raze #1,
      2026-11-06, A76, 1280x800 GREY sources 8 in turn (cold), 2336 interleaved runs: p50 137.4-137.8
      µs made then read, 137.4-139.6 µs banded, LLC read misses 14 300-14 450 either way. The 256 KB
      level stays in the A76's 512 KB L2 between being made and read, and the time is the cold 1 MB
      source. Prototype not kept (no API without a gain).
- [x] **UVC onto Lemnos's one USB transport.** `styx-uvc` claims, controls and streams through
      `lemnos-usb` (`UrbRing`, Lemnos d190650); Styx has no usbfs code. Three USB stacks
      (Lemnos rusb, Atlas nusb, Styx usbfs) become one. Raze #1, 2026-11-03, webcam 1bcf:0050
      (isochronous), MJPG 1280x720 and YUYV 640x480 at 30 fps, 300 frames each: 0 sequence gaps, 0
      damaged, 300 of 300 timestamps from PTS, interval sd under 0.001 ms against 1.494 for V4L2 on
      the same camera, `uvcvideo` back on both interfaces after each detach, 0.000 allocations a
      frame. Bulk is still untested: no bulk camera at the bench.
- [x] **One Vulkan device and queue per process** (aa95fb4). With `daedalus`, the GPU ISP takes the
      process's device (`GpuContext::shared`), and `GpuConvert` submits there too, so the GPU JPEG
      and H.264 encoders can run on it (their callers supply the context). `one_device.rs` asserts
      `devices_created() == 1`; on V3D with the validation layer and sync validation, 0 messages.
      Without the feature gpuisp makes its own device; `STYX_GPUISP_DEVICE` does too; the fallback
      is [The leap](#the-leap-started-2026-11-06) item 5. fp16 pictures stay off on Daedalus's
      device.
- [x] **Inactive controls and unread frames** (eb27ac1, 1ec5264). `ControlDescriptor::inactive`, and
      `ControlRefusal::Inactive` for a V4L2 control the driver has turned off: written, untested on
      board, V4L2 only (userspace UVC does not report it). `Drops::unread` counts queue overflow
      with no reader apart from `total`.
- [x] **Uevent hotplug on Lemnos's.** Already done: `styx::watch` (`watch/linux.rs`) and
      `styx-uvc`'s `Hotplug` both use `lemnos_linux::uevent`. Styx has no netlink code of its
      own; I²C, GPIO and embedded-hal are Lemnos's as well. The
      HeliOS item "Styx's uevent watcher moves onto Lemnos's" has nothing left to do.
- [ ] **dma-heap, dma-buf and V4L2 probing all on Lemnos.** The heap open and allocate, CPU-access
      syncs, `dmabuf_id` and the dma-buf length are Lemnos's (`lemnos_linux::dma`,
      `lemnos_linux_sys::dma_buf`, 9b3e835); `styx_native::dma` keeps only what is Styx's (the
      allocation padding `TAIL_SLACK`, the memfd stand-in). The audit of 6a6f848b found three
      raw calls left: the FFmpeg codec probe's `VIDIOC_QUERYCAP`/`ENUM_FMT` and the VA-API test's
      dma-heap allocation now go through Lemnos (2026-11-07); `pipewire-styx`'s `UDMABUF_CREATE`
      waits on a Lemnos udmabuf API (asked 2026-11-07). *Done when:* no `libc::ioctl` on a device
      outside Lemnos in Styx (`grep -rn "libc::ioctl" crates` empty). *Blocked by:* Lemnos's
      udmabuf call. Owed: the Raze run of the allocations and syncs was done on the software-ISP
      path only.
- [x] **V4L2 M2M session onto Lemnos.** `styx_codec::m2m::M2mSession` keeps its API (copy,
      row-by-row and dma-buf import pushes, held frames, pull, drain) over Lemnos's
      `v4l2::m2m::M2mSession` (Lemnos c546b44): the source format is set first, then the
      destination's, read back before the buffers (vicodec sized its coded buffers from the old
      order). Controls go through `device()`, a duplicate of the session's open file. vim2m and
      vicodec tests pass. RGA is Lemnos's (`lemnos-drivers-rga`); Styx's M2M is for codecs.
      Decoders (Lemnos 771217a's source-change flow) are not on M2M in Styx yet.

- [x] **ov9282 and pisp_be patches dropped** (user decision, 2026-11-03): `kernel-modules/ov9282`,
      `kernel-modules/pispbe`, their packaging and the `queue_safe` check are gone. Pass jobs run one
      at a time, the 1280x480 mode is gone, and the stock driver has no watchdog for a stuck job.
- [x] **Queued PiSP passes on the Raze** (220aff5, a083112). Raze 2026-11-02 (HeliOS dev.844,
      patched module): `queue_safe` reads `Y`; `native-pipeline pisp --passes` with the default
      `--pass-jobs 0` over 150 frames at 30 fps: 0 "not safe" messages, 0 skipped, pass time
      0.122 ms a frame (p95 0.146), 0.062 ms a job, both regions bit-exact against the main
      output's crop.
- [x] **Zero copy: Daedalus importing software ISP frames on the CM5** (8b1daa4, 93ed532 output
      path; `daedalus_import_check`, this commit). Raze 2026-11-02, run by Daedalus 3/3: a
      1280x800 NV12 frame from the software ISP's dma-heap buffer imported with
      `GpuStorage::import_dmabuf` in 95-97 µs, 0 of 1536000 bytes differ on the GPU's read, and a
      sentinel written through the CPU mapping reads back on the GPU (the same memory).
- [x] **GPU import pad from the device** (measured on the Raze 2026-11-02, Styx d9fd78a). `GpuContext::import_pad(len)`
      asks the GPU ISP's device what a dma-buf needs past its size; the native software ISP path
      pads its output buffers to at least that when it has a GPU ISP device. A Daedalus consumer
      takes it from `GpuStorage::dmabuf_import_len` and sets `capture_gpu_import_pad`; other
      paths keep the configured 256. *Why:* the fixed 256 B is v3dv's need, not every
      device's. *Numbers:* `import_pads_are_what_the_device_says` reads 256 on the Raze's v3dv (V3D
      7.1.10.2, HeliOS dev.844); 0 on RADV (RX 6800 XT), NVIDIA (RTX 4060) and llvmpipe.
- [x] **Styx's own H.264 encoder** (d7807c0 encoder, 7c0d27e RVV leaves, 83a383d GPU search,
      d6c9686 `StyxH264Encoder` registered without `codec-ffmpeg`, 49a990b rate control, this
      commit quarter-pel motion): Baseline CAVLC, decoding bit for bit to its reconstruction
      under FFmpeg. 1280x720 QP 26, dev PC under load ~45: textured scene moving 0.5 / 1.25 / 3
      px a picture, P bytes 30796 → 5799, 38024 → 7165, 25736 → 9459 with quarter pels (time
      within noise of whole-pel at that load); a static-noise scene with a moving square, 700 →
      767 bytes (+10 %), `set_subpel(false)` keeps whole pels. Rate control: 150 and 600 kb/s
      held to +0.3 % and +0.2 % over 300 noisy 176x144 pictures.
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

### Now list of 2026-11-03

- [x] **Push 77e3160 to dev once gated** and send the commit: pushed with this file.
- [x] **Measure `rp1-cfe`: what sliced frames are worth on the default pipeline.** Raze #1, HeliOS
      dev.921, the live camera service at 30 fps, a low-priority client, `Frames::gray()` 1280x800,
      300 frames twice each. Complete frames received p50 8.47-8.48 ms (p90 8.73-8.76, p99
      10.1-10.6); sliced frames p50 6.14-6.15 ms (p90 6.34, p99 7.7-7.9) with 552 rows final, and
      complete at p50 8.65-8.66 ms (p90 8.9). The first rows are 2.33 ms earlier and the whole frame
      0.18 ms later. This is delivery, not detection latency: the detector A/B was not run, since
      the graph always takes slices and the board's default pipeline was left alone. From the
      signal side at 120 fps: slicing takes a complete frame from 9.30 to 8.14 ms for 0.03 ms of
      CPU; stock `FRAME_SYNC` plus a timer at 5.85 ms gives the same 8.14 ms for 0.015 ms, 0 of 1201
      early at 120, 60 and 30 fps (5.75 ms: 3 early; 6.25 ms: flat; 6.65 ms: 8.48; 7.05 ms: 8.87).
      Row 599 lands 5.727 ms after frame start. What an own receiver must match: 552 rows by about
      6.15 ms, the whole frame by 8.1-8.5 ms, a line signal by 5.74 ms.
- [x] **The bridge from the 2026-10-02 A/B** (kernel 6.12, not re-measured): reported to the
      coordinator. HeliOS does not need it. It would end the device package's two OV9782 patches
      only if every variant moved, and the libcamera variants still use the `ov9282` driver.
- [x] **The timer path replaces the patched line event** (user's go, 2026-11-03; `rp1-cfe` module
      and package removed). Stock `FRAME_SYNC`, no thread: a 16-byte marker starts the row after
      the slice line in every raw buffer before it is queued, and at `line × line_time + 0.4 ms`
      from the sensor's mode a marker gone raises the line event; one still there runs the frame
      whole. Raze #1, 7.2.9, line 600 of 800 at 9.1 µs lines (delay 5.86 ms), 1200 frames:
      complete frames 8.14 ms (p95 8.25) at 120 fps against 9.30 whole, 8.15 at 60, 8.08 at 30, 0
      early in all of them. RAW8 at 7.58 µs lines, line 400 (delay 3.43 ms), through a camera
      service at 120 fps with a raw client: 962 frames, 0 early. Under load (4 spinning threads on
      the 4 cores, 120 fps): 0 early; 1171 of 1200 sliced, the other 29 lost to late wake-ups (23
      replaced by the next frame's start, 6 a frame behind), worst 1.79 ms late; 8.15 ms (p95 9.68)
      against 9.31 (p95 11.09) whole. Pixels, `--no-tdn`, every 8th frame re-run from its final raw
      buffer: 729 frames, 282 of them under load, byte-identical to one job with the same stripes,
      so no job read a row before it landed. Against one stripe, the unsliced path, every frame
      differs around the stripe's edge (about 28000 of 2.3 M bytes, at most 14 codes): the tiling,
      not the timing; the old event used the same stripes. Stock node, patched module still in the
      slot; `FRAME_SYNC` there is the import's, unchanged.
- [x] **Finish the Raze list.** 2571ba4 gated and pushed. No-reader overflow count: `Drops::unread`,
      Raze #1 2026-11-03 on a V4L2 camera with `pause_when_idle`: 0 with a client, 13 in 3 s with
      none, still 13 with the client back, `total` 0 throughout. UVC run: moved to
      [Done](#done). Sync validation of the 0-message run: Debian's layer 1.4.309 loaded through
      `LD_LIBRARY_PATH`, the control run reports 1 `SYNC-HAZARD-WRITE-AFTER-WRITE`, `sync_false` and
      `one_device` 0, exit 0. `Submit::Own` review: item 5 below.

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
- [x] Pure-Rust kernel interfaces: V4L2, media controller, subdevs, events, dma-heaps; first in
      Styx, now Lemnos's (`lemnos-v4l2`, `lemnos-linux`). (df84c54, 7fe3448, 6d1d4a0, 4dceae2)
- [x] V4L2 backend in pure Rust; the `v4l` crate is gone everywhere. (d23196a)
- [x] libcamera fully optional: no default build pulls it, CI job without it, checks in
      `scripts/check-feature-combinations.sh`. (97aac59)

### Sensors
- [x] Sensors as data (`styx-sensor`, TOML descriptions). (31bacba, 9c4f745) The data moves to
      Lemnos with its sensor modules (OV9782 first).
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
- [x] Legacy frame socket with leases and its public codec: removed for 3.0 (the camera service
      and `FrameClient` replace them).
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
- [x] Default PiSP output buffer count: 6 (two socket consumers holding 500 ms: 17 fps
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
      kernel messages, raw and MJPEG decoders, imported frame layouts, camera service messages
      and requests, recordings, raw recordings), `scripts/fuzz.sh`, nightly CI
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

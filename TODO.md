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

## Alpha (Raze)

The next release is an alpha for the Raze (CM5, OV9782, PiSP) only; the other boards follow
right after it. These items come first. Paused and held items stay paused or held until the
user lifts them.

- [ ] **GPU ISP on the Pi 5: timing measured, too slow** (import and output verified on the Raze's v3dv at eeefd6d:
      dma-buf import and export pass, Daedalus buffer path and equivalence bit-exact, 0 validation
      messages; again at d9fd78a on HeliOS dev.844, VVL 1.4.309 with sync validation). The replay
      source's software ISP does not use it: only live native captures do (`native_isp_bench single
      FPS FRAMES --soft-isp`). *Why:* an estimate is all we have (`crates/gpuisp/README.md` and
      docs/native-stack/pipeline.md say so). *Done when:* ms per 1280x800 frame and CPU measured
      there and written into both. *Blocked by:* nothing: measured, now
      the shader's 35 ms needs profiling, after the other alpha items; the GPU ISP is off by
      default on V3D meanwhile (`STYX_GPU_ISP=1` takes it). *Numbers:*
      2026-11-02, live 1280x800 at 60 asked: 28.3 fps (~35 ms GPU), latency 133 ms, CPU 1.97 ms
      a frame; CPU software ISP 59.98 fps, 12.3 ms, 9.5 ms CPU. Estimated 4-8 ms GPU against
      2.6-3.0 ms fp16 NEON. `styx-wgpu`'s pyramid on the same
      GPU: 1.67 ms p50 and 0.38 ms CPU for 3 levels (42fb407).
- [ ] **PiSP back end ops for Eidos on the Raze** (`styx_pisp::be::ops` + `device::BeImageDevice`:
      written, untested on board). *Why:* Eidos lowers resample/downscale (pyramids), tone LUTs,
      5x5 filters and demosaic + CCM onto the BE memory to memory on its own dma-bufs. *Done when:*
      each of `styx.pisp.{resample,downscale,lut,filter5x5,demosaic_ccm,box2,pyramid2}` runs on
      the CM5 and its exactness is measured: downscale, box2 and pyramid2 against
      `(a+b+c+d+2)>>2` (which codes differ, if any, and whether the rounding truncates), resample
      against the CPU reference (max abs error), the LUT against `LutFit`, the 5x5 and demosaic
      as tolerances; GREY in through the greyscale Bayer path confirmed accepted by
      `pispbe-input`; `run2` writing both outputs from one job; whether output 0 has a
      downscaler after all (libpisp says output 1 only). *Blocked by:* the GREY input: `be_ops_check` (crates/pisp/examples)
      on the Raze 2026-11-02 fails every GREY op at `VIDIOC_REQBUFS` (EINVAL), so the greyscale
      Bayer path or its buffer setup needs fixing first. *Numbers:* `demosaic_ccm` with the
      identity matrix: 0.223 ms a 640x480 job, four flat RGGB patches come out exactly.
- [ ] **Zero steady-state allocations: what isn't counted, or isn't 0.** *Why:* standing rule for
      every dependency: a count of 1-3 per frame is a finding; assert 0, or state the physical reason.
      *Done when:* every per-frame path below asserts 0, or names its reason. *Blocked by:* nothing for the PiSP path; a
      device for V4L2 and libcamera. Raze 2026-11-02, `hop_breakdown inproc` (PiSP NV12
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
      box and the reactor's timer map node). **Not counted:**
      V4L2 zero-copy (uses the pool, its loop needs a device), libcamera, and the kernel's side
      of the queue on a board. Also asserted 0: the PiSP raw lease, whole and at a sliced frame's
      first slice (`a_raw_lease_allocates_nothing`), and the sliced frame's states, reused once
      their leases return (`slice_states_are_reused_once_free`; were 4 `Arc`s per frame). GPU ISP (`crates/gpuisp/tests/it/
      alloc.rs`): 0 with statistics handed back (`recycle_stats`) and white balance, gain, lens
      shading and tone changing every frame (`swap_params`, tables rebuilt in place); was a new
      `IspStats` and new tables per frame. `Input::Buffer` on Daedalus's device: 0, submitted
      through Daedalus's `RawSubmitter` (Daedalus fd18808); was 17 per frame in `submit_raw`
      and wgpu-core's submission.
- [ ] **`native-pipeline regcheck`: `0x0101` and `0x1000` read back 0.** *Why:* docs/portability-design.md
      still says all 93 identical. *Done when:* they are excluded as write-only or self-clearing, or the
      description is fixed. *Blocked by:* a Raze run with the OV9782's description: on 2026-11-02
      `regcheck` without `--description` wrote 0 registers, so nothing was checked. *Numbers:* 2 of 93 differ.
- [ ] **Still JPEG encoding numbers.** *Why:* the default features fall back to the `image` crate. *Done when:*
      the ~45 ms still thread time is re-measured with turbojpeg. *Blocked by:* a still that lands: on the Raze 2026-11-02
      `still_capture DIR 30 native` (turbojpeg) failed with "still: timed out". Likely cause, fixed
      and written, untested on board: `settle(true)` armed the shot only once AE locked, so a scene
      AE never locks on waited out the 5 s; it now waits half the timeout, then takes AE's exposure. *Numbers:*
      ~45 ms, before turbojpeg.
- [ ] **CM5 HEVC decode check** (`hevc_v4l2request` through FFmpeg). *Why:* HEVC recordings only; Styx
      records MJPEG and raw. *Done when:* a recording decodes through `rpi-hevc-dec`. *Blocked by:*
      an Atlas image with that driver; HeliOS says when. *Numbers:* none: the driver is not in the
      image.
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
      *Blocked by:* a K230D board; the planner has no M2M scaling stage yet (see the RGA item),
      so nothing picks the converter for pyramid levels or preview on its own.
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
- [ ] **Rockchip: RGA scale, crop and convert** (RGA3 on RK3588S, RGA2 on the others). *Why:* deeper
      pyramid levels and preview frames without the CPU. The stack's one RGA binding is Daedalus's
      `daedalus-rga` (agreed with Daedalus 2026-11-02): V4L2 M2M for mainline `rockchip-rga`, librga
      for RGA3, no rotation yet. Styx has no RGA code of its own; `V4l2M2mConverter` is not for RGA.
      Written, untested on board: `styx_codec::RgaConverter` (feature `codec-rga`), dma-buf frames
      read in place, host frames through dma-heap buffers, scale/crop/convert tests against vim2m
      (identity and channel swap exact). Format conversion is hooked up: `CodecKind::Converter`, the
      registry offers RGA conversions between NV12, NV21, YU12, RG24, BG24, BGRA and GREY when a
      `rockchip-rga` node exists, the planner takes them where it takes a raw decoder (hardware first,
      `Hardware::Off` falls back to the CPU), and encoder lookups never take them. Not written: an M2M
      scaling stage in the planner (registry converters keep the frame's size). *Done when:* the
      planner scales through RGA within a stated tolerance, CPU fallback without it; then the board
      run. *Blocked by:* the scaling stage; the board run needs a Rockchip board.
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
- [ ] **pispbe patch: carried** (user decision: we keep carrying it; the drafts in
      `kernel-modules/pispbe/README.md` stay unsent, nothing gets filed). *Why:* it takes the config
      write from 117 to 8 µs per job and makes queued jobs safe; the stock driver overwrites a held
      job. A kernel bump stops the image build until the patch is redone. *Done when:* the user
      decides otherwise; until then this is a standing cost, not work. *Blocked by:* the user.
      *Numbers:* Styx API 0.35 → 0.23-0.26 ms/frame at 30 fps; stock driver 455 "not safe" in 5 s.
## Open: every stage on every backend, and allocations

- [ ] **Every stage accelerated on every backend** ([docs/acceleration.md](docs/acceleration.md)).
      *Why:* user rule: every stage that can be accelerated is, on x86, NEON64, armv7 NEON, RVV, the
      GPU and each board's fixed-function units; losing variants stay behind knobs. *Done when:* the
      matrix has no gap cells; each leaf conformance-tested against scalar (guard bytes, odd widths)
      in the gate, on hardware or QEMU. *Blocked by:* nothing for the CPU and GPU columns; boards
      for the fixed-function rows. *Numbers:* recounted from the matrix: **230 pass, 27
      gaps, 1 n/a of 258 cells** (43 stages). Gaps by column: x86 1, NEON64 0, NEON32 0, RVV 2, GPU 24.
      The CPU gaps: fp16 on x86 and RVV, and JPEG decode on RVV (JPEG and H.264 encode: Styx's
      own encoders with RVV leaves, written, untested on board). The GPU gaps: every
      stage of `post.rs` (5), the packed conversions (11), YUYV → NV12 chroma, NV12/YUYV/I420 → RGB
      (3), the edge-directed demosaic, the polynomial tone curve, fp16, and JPEG decode. Fixed-function:
      5 pass, 7 gaps, 6 of which need a board. armv7 and RVV leaves now cover every kernel in
      `styx_core::simd` and every integer kernel of `styx_softisp::simd`.
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

Not on the freeze list. *Numbers:* none measured unless stated.

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
- [ ] **rp1-cfe device-tree node leak** (upstream). *Why:* one node per runtime overlay up/down; dev runtime
      path only. *Done when:* fixed upstream. *Blocked by:* upstream.
- [ ] **Deflicker corners.** *Why:* rolling-shutter band gains are unverified on a sensor (needs `readout` from
      its description); 30 fps with short exposures leaves ~2%; with deflicker off, a clipped lamp makes AE
      chase the beat at 120 fps (simulated). *Done when:* verified on a sensor, or accepted by the user.
      *Blocked by:* the bench pause. *Numbers:* as stated.
- [ ] **Reject impossible frame-length reports.** *Why:* a value from embedded data is trusted even when
      impossible (VTS 4 on an 800-line mode). *Done when:* the control schedule rejects such reports.
      *Blocked by:* nothing.

## After the release: dependency layers (slated, not started)

HeliOS's plan (HeliOS architecture-overhaul `TODO.md`, "After the release: dependency layers",
2026-11-02). Each item needs the user's go before it starts. No code moves before then.

- [ ] **UVC onto Lemnos's one USB transport.** `styx-uvc` claims, controls and streams through
      `lemnos-usb` (`UrbRing`, Lemnos d190650); `styx-kernel` has no usbfs module. Written, untested
      on a camera. *Why:* three USB stacks (Lemnos rusb, Atlas nusb, Styx usbfs) become one.
      *Done when:* UVC's no-drop and timestamp checks, a detach and reattach of `uvcvideo`, and the
      steady-state allocation count pass on a real camera. *Blocked by:* a board slot (SystemCore
      C270). *Numbers:* none yet.
- [x] **Uevent hotplug on Lemnos's.** Already done: `styx::watch` (`watch/linux.rs`) and
      `styx-uvc`'s `Hotplug` both use `lemnos_linux::uevent`. Styx has no netlink code of its
      own; I²C, GPIO and embedded-hal are Lemnos's as well (`styx-kernel/src/bus.rs`). The
      HeliOS item "Styx's uevent watcher moves onto Lemnos's" has nothing left to do.
- [x] **dma-heap and dma-buf basics onto Lemnos.** The heap open and allocate, CPU-access syncs,
      `dmabuf_id` and the dma-buf length are Lemnos's (`lemnos_linux::dma`, `lemnos_linux_sys::dma_buf`,
      9b3e835). `styx_kernel::dma_heap` stays as a thin layer for what is Styx's: the allocation
      padding (`TAIL_SLACK`), the sync counters (`sync_stats`) and the memfd stand-in. Owed: the
      Raze run of the allocations and syncs was done on the software-ISP path only.
- [ ] **One Vulkan device and queue per process.** Done on the host (aa95fb4): with `daedalus`,
      the GPU ISP takes the process's device (`GpuContext::shared`), and `GpuConvert` submits there
      too, so the GPU JPEG and H.264 encoders can run on it (their callers supply the context).
      `crates/gpuisp/tests/one_device.rs` runs an engine, the ISP and a conversion and asserts
      `devices_created() == 1`. Without the feature gpuisp makes its own device, the no-Daedalus
      build; `STYX_GPUISP_DEVICE` does too. fp16 pictures stay off on Daedalus's device. *Done
      when:* the test and the validation layers pass on V3D. *Blocked by:* a board slot (Daedalus
      runs it).
- [x] **V4L2 M2M session onto Lemnos.** `styx_kernel::v4l2::M2mSession` keeps its API (copy,
      row-by-row and dma-buf import pushes, held frames, pull, drain) over Lemnos's
      `v4l2::m2m::M2mSession` (Lemnos c546b44): the source format is set first, then the
      destination's, read back before the buffers (vicodec sized its coded buffers from the old
      order). Controls go through `device()`, a duplicate of the session's open file. vim2m and
      vicodec tests pass. RGA is Daedalus's (`daedalus-rga`); Styx's M2M is for codecs.
      Decoders (Lemnos 771217a's source-change flow) are not on M2M in Styx yet.

## Done

Numbers are as of the commit named.

### Since the last audit
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

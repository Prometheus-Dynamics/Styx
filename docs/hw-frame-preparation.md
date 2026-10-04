# Hardware-Accelerated Frame Preparation: Investigation

Status: research 2026-09-25; items (0), (a), (b) and the core API of the ranked plan implemented
2026-09-26 (see [Implemented](#implemented-2026-09-26)). Branch: `dev` @ `54ff6e1` + working tree.

Goal: frames should reach Eidos already in the shape it wants: Y8, stride-aligned, optionally with a
½ or ¼ companion image and an optional ROI, at zero or near-zero CPU cost.

**Evidence labels used throughout**

- **[M]** Measured on the x86 dev host (details in [Measurements](#measurements)).
- **[M-CM5]** Measured on the `helios` CM5 (Cortex-A76 @ 2.4 GHz, OV9782 CSI sensor, libcamera
  0.6.0+rpt, kernel 6.12.47), see [On-device measurements](#on-device-measurements-helios-cm5).
- **[C]** Read from the Styx source (file references given).
- **[D]** From vendor docs or general platform knowledge. Not verified on hardware here. Items
  marked *verify* are the ones I'm least sure about and should be checked on the device first.
- **[E]** Estimate derived from [M] with a stated scaling factor.

Update 2026-10-04: regions of interest are a companion kind now, as section 5.1 proposed
(`CompanionKind::Region { index }`, the actual rectangle in each region's `FrameMeta::crop`):
`FrameRequest::regions` asks for up to 16 per consumer, cut from one capture. On a native
camera's PiSP they come from the main output's crop, the second output's, or extra back end
passes over the same raw frame (0.05 ms of back end time and ~0.03 ms of CPU per 128x128
region on the CM5); elsewhere luma frames get views, and the overview is box-filtered on the
CPU. See [frame-planning.md](frame-planning.md#region-of-interest).

---

## Implemented (2026-09-26)

All numbers below were measured on `helios` (CM5, OV9782 1280×800, libcamera 0.6) through the
real Styx `CaptureRequest` / `Codec` APIs.

| Change | Where | Result on CM5 |
|---|---|---|
| Cached dma-heap capture buffers + `DMA_BUF_IOCTL_SYNC` (default on for Raspberry Pi, falls back to libcamera's allocator; `STYX_LIBCAMERA_BUFFER_MEMORY`) | `libcamera_backend/heap.rs`, `backing.rs`, `core/buffer/dmabuf_sync.rs` | Y-plane row scan 0.67 → 0.40 ms (1.7×); strided column read 3.04 → 0.80 ms (3.8×); 0 sequence gaps |
| Zero-copy luma: `luma_rows`, `into_luma`, `has_luma_plane` (works on mapped dma-buf frames) | `core/buffer/frame/luma.rs` | Y8 view of NV12/YUV420 with no copy |
| GREY/R8 on colour sensors: capture YUV420, deliver Y plane as GREY | `libcamera_backend.rs` | `R8` request now yields zero-copy GREY 1280×800, stride 1280 (was silently raw Bayer) |
| Pyramid companions on `FrameLease` (`CompanionKind::Pyramid`, `pyramid_level`, `with_box_pyramid`) | `core/buffer/frame/companion.rs` | Software 2×2 box: 0.16 ms for 1280×800 → 640×400 |
| ISP pyramid companion from the PiSP second output (`CaptureRequest::luma_pyramid(level)`) | `libcamera_backend.rs`, `backing.rs` (shared request ownership) | ½, ¼, ⅛ all work, NV12 or GREY; 120/120 frames with identical timestamps; same-frame ISP ½ vs software box differ by 0.78 grey levels on average |
| `TurbojpegLumaDecoder` (MJPG → GREY, 64-B rows; scale, crop, fast DCT, pyramid options) | `codec/mjpeg_turbojpeg_luma.rs` | Real 800p 4:2:2: 13.16 ms (`jpeg-decoder` RGB) → 2.36 ms; 1080p: 27.97 → 5.60 ms; centre-¼ crop 1.26 ms |
| `LibcameraFrameMeta { sequence, buffer_memory }`, `FrameMeta::sequence()` | `core/buffer/meta.rs` | Sequence numbers now reported for libcamera |
| V4L2 GREY/R8 stride fix + zero-copy | `v4l2_backend.rs` | GREY frames no longer dropped (unit-tested; no GREY UVC camera on hand) |
| Fallback RG24 / luma conversion prefer turbojpeg over `jpeg-decoder` | `runtime_codec.rs`, `frame_image.rs` | — |

**USB MJPEG camera (Logitech C270, `046d:0825`, V4L2, on the CM5):**

| Mode | JPEG size | old `jpeg-decoder` RGB | turbojpeg RGB | turbojpeg-luma GREY + ½ pyramid |
|---|---|---|---|---|
| 1280×720 | 29 KiB | 9.88 ms | 3.83 ms | **2.11 ms** |
| 960×544 | 17 KiB | 6.05 ms | 2.16 ms | **1.21 ms** |
| 640×480 | 10 KiB | 3.15 ms | 1.23 ms | **0.61 ms** |

- Before the fix, both turbojpeg decoders rejected C270 frames ("extraneous bytes before marker",
  a libjpeg *warning*). They now accept them: 0 failures in 450 frames.
- Every frame is 4:2:2 and carries restart markers, so multi-core slice decoding is possible.
- The camera ran at 11.7–12.9 fps despite 30 fps being negotiated. Turning off V4L2
  `exposure_auto_priority` ("Exposure, Dynamic Framerate", `0x009a0903`) gave 24.7 fps.

**Correction to the research numbers below:** the "10.2 ms uncached" figure came from a scalar C
byte loop. Rust code that the compiler vectorises reads uncached PiSP buffers much faster
(0.67 ms). The real penalty is 1.7× for sequential SIMD-friendly scans and 3.8× (up to ~15× for
pure scalar loops) for strided or random access, which is what contour following and marker
sampling do.

**Found along the way:**
- libcamera-rs 0.7 `OwnedFrameBuffer::new` leaks the plane fds it is given, because libcamera
  duplicates them. `heap.rs` closes them explicitly. Worth reporting upstream.
- A process that exits without stopping the libcamera manager leaves its
  `raspberrypi_ipa_proxy` running, reparented to init. That happens when
  `STYX_LIBCAMERA_STOP_WHEN_IDLE`/`stop_when_idle` is off, which is the default. This was not
  changed here.

**Second round (2026-09-26), also verified on `helios`:**

| Change | Result |
|---|---|
| Multi-core MJPEG decode via restart-marker slices (`LumaDecodeOptions::threads`) | C270 720p 1.59 → 0.88 ms, 640×480 0.52 → 0.36 ms; 60/60 frames byte-identical; JPEGs without restart markers fall back unchanged |
| FFmpeg hardware decode (`FfmpegHwDevice`, `FfmpegLumaDecoder::best_available`) | Compiles and lints against FFmpeg 8 (aarch64); on the Pi 5 it correctly finds no usable hardware (no JPEG/H.264 block) and falls back; **not yet tested on RK3588, x86 VA-API or Jetson** |
| DRM-PRIME frames mapped for CPU reads, with dma-buf sync | Enables zero-copy Y8 from MPP/V4L2 M2M decoders; untested without that hardware |
| `ScalerCrop` / `ScalerCrops` rectangle controls | Set (320,200)/640×400 through Styx; ISP applied it within 5 frames |
| libcamera manager stopped at exit | 0 orphaned IPA helpers after exit (was 1 per process) |
| NV12/I420 on single-planar V4L2 | Unit-tested; no NV12 UVC camera on hand |

**Still open:** V4L2 multi-planar capture with dma-heap import (needed for RK3588 rkisp and i.MX CSI),
RGA/VIC scalers, on-hardware validation of the FFmpeg backends, and the `FrameRequirements` planner.

---

## TL;DR

1. **On the CM5, Styx's zero-copy libcamera frames sit in uncached memory [M-CM5].** Styx gets
   buffers from libcamera's `FrameBufferAllocator` (`libcamera_backend.rs:338`), and those are
   `videobuf2_dma_contig` buffers that the CPU maps *uncached*.
   - A scalar byte loop over the 1 MB Y plane takes **10.2 ms**. NEON takes 0.59 ms, memcpy 0.67 ms.
   - Importing buffers from the **cached** dma-heap (`/dev/dma_heap/linux,cma`) into libcamera
     works, and brings this down to **0.69 ms byte loop / 0.04 ms NEON**.
   - Cached buffers then **need `DMA_BUF_IOCTL_SYNC`** (~55 µs per MB for START+END). Without it I
     measured stale reads. Styx has no sync today.
   - Any per-pixel scalar work in Eidos on today's Styx CSI frames therefore runs ~15× slower than on
     heap memory. This is the first thing to fix.
2. **The biggest MJPEG win is not hardware.** Styx's default MJPEG path decodes to RG24 with
   `jpeg-decoder` [C], and Eidos then converts that to luma. On the CM5, decoding straight to GREY
   with libjpeg-turbo is **4.9–9.0× faster [M-CM5]** (2.2–3.4× on x86 [M]). It needs no new
   dependencies, and it is the only MJPEG option on a Pi 5, which has no hardware JPEG decoder [D].
3. **Luma-only and DCT-scaled decode save much less than hoped.** Huffman decoding is the floor: a
   ⅛-scale decode still costs 50–70% of a full grayscale decode on the CM5. Skipping chroma saves
   20–25% on 4:2:2 and 10–18% on 4:2:0 versus YUV output, and 33–47% versus RGB [M-CM5].
4. **A software pyramid is almost free; a second decode is not.** Full GREY plus a 2×2 box filter
   adds 0.06–0.07 ms at 720p/800p and 0.16–0.19 ms at 1080p on the CM5 (+1–4%). A second decode at ¼ adds 65–85%
   [M-CM5].
5. **The PiSP dual-output pyramid is confirmed on hardware [M-CM5].**
   - Full-res NV12 plus a ½, ¼ or ⅛ NV12 stream all validate. 150/150 frames arrived with both
     buffers and identical timestamps and sequence numbers.
   - Strides are 64-byte aligned (a 160-px row gets a 192-byte stride).
   - There's no R8/Y8 ISP output for a colour sensor (libcamera rewrites R8 to raw Bayer). Plane 0
     of NV12/YUV420 is the Y8 view.
   - `ScalerCrop` takes effect 4 frames after it's set, and the per-output `ScalerCrops` exists.
   - Styx's TDN code already configures two streams.
6. **Hardware MJPEG decode helps on weak CPUs, but only if we avoid the download/convert step.**
   On the x86 host's AMD VCN, VA-API decode left 0.26–0.4 ms of CPU per frame [M]. A generic
   "download to NV12" path cost as much CPU as software decode for 4:2:2 streams [M], and most UVC
   cameras send 4:2:2.
7. **A generic V4L2 M2M path won't cover the targets we ship** [D].

**Revised build order** (this challenges the original guess of "HW/cheaper MJPEG, then pyramids"):
**(0)** cached, synced libcamera buffers; **(a)** software MJPEG→Y8 with a software pyramid;
**(b)** the libcamera dual-stream pyramid plus a Y-plane view; **(c)** hardware MJPEG per vendor.
Full reasoning is in [Ranked plan](#4-ranked-plan).

---

## 1. What Styx has today

### Frame model ([C] `crates/core/src/buffer/frame.rs`, `meta.rs`, `format.rs`)

| Area | Current state | Gap for Eidos |
|---|---|---|
| `FrameLease` | Owned (pooled `Vec<u8>` per plane) or external (`Arc<dyn ExternalBacking>`, read-only). `PlaneLayout{offset,len,stride}` per plane. `visible_rows(i)` hides stride padding. | None for Y8 itself. **No companion or extension slot**, and `validate_plane_layouts` rejects extra planes (frame.rs L832). |
| Formats | `FourCc` is an open 4-byte struct. `GREY` and `R8` both exist (1 bpp, Luma). NV12/NV21 and I420/YU12/YV12 are fully described. `FourCc::layout_info()`. | No `Y8` alias (GREY is the V4L2 name, which is fine). No NV16 constant. |
| Stride alignment | `FrameAllocation{stride_alignment, plane_alignment}` exists, host-owned only (L333, L638). `BufferPool` hands out plain `Vec<u8>` with no base alignment. | Decoders don't request alignment. Base alignment can be done via `PlaneLayout.offset` without a new buffer type. |
| Metadata | `FrameMeta{format,timestamp,backend,capture_instant,residency,…}`. `BackendFrameMeta` has only a `V4l2` variant (sequence lives there). | libcamera sequence and sensor timestamp are not propagated. No place for ROI rect or scale factor. |
| Cache sync | **No `DMA_BUF_IOCTL_SYNC` anywhere** (grep). libcamera and shared-fd dmabufs are `mmap`ed and read directly. | Works today only because the libcamera buffers are uncached, which is slow. Mandatory once buffers are cached ([M-CM5](#cm5-2-cpu-access-to-pisp-output-buffers)). |
| Negotiation | `CameraRequest::format_priority/resolution_priority/…` scores (format, res, backend, fps, area) (`capture_api/request/camera.rs` L485). The default order ranks R8/GREY *last*. `StyxPathRequest`/`explain_styx_path` explains one capture→codec hop. | No way to say "I want luma, 64-B stride, ½ pyramid, this ROI". |
| Pipeline | `MediaPipelineBuilder`: capture → ≤1 decoder → hooks → ≤1 encoder. | A "prepare" stage (decode + pyramid) would fit where the decoder sits today. |

### Capture backends ([C])

**libcamera** (`crates/styx/src/capture_api/libcamera_backend.rs`)
- Zero-copy dmabuf `FrameLease::from_external` with `LibcameraBacking`, which holds the Request and
  requeues it on drop (backing.rs L395). NV12 is exposed as 2 planes.
- Buffers come from `FrameBufferAllocator` (L338). On PiSP these are `videobuf2_dma_contig` exports
  and are **uncached for the CPU** [M-CM5].
- Uses one stream by default (role ViewFinder). **A second stream is already configured when TDN
  output is enabled** (L186–189, L230–240), but it is forced to the *same* format and size, and only
  one of the two buffers is emitted per Request (L552–566).
- No ScalerCrop support: Rectangle controls are mapped to `Unknown` and skipped (`crates/libcamera/src/lib.rs` L284).
- **Bug:** `stream_role_for_request` maps `GREY` to `StreamRole::Raw` (util.rs L165–178). Asking
  PiSP for GREY therefore requests the raw Bayer stream, not luma.

**V4L2** (`v4l2_backend.rs`)
- MMAP only, single-planar, 4 buffers. Zero-copy only for MJPG/JPEG/YUYV/RGB. Other formats are
  copied into a memfd pool. No MPLANE, no M2M, no media controller, no `S_SELECTION`.
- **Bug:** `min_stride_for_fourcc` has no `GREY`/`R8` arm and falls back to `width*3` (L461–481).
  A GREY camera then fails layout planning and every frame is dropped. The same code path also looks
  wrong for single-plane NV12 (the stride is computed as ~1.5w and it doesn't produce 2 layouts).
  Both come from reading the code and were not run.

**Codec** (`crates/codec`)
- Four MJPEG decoders: `jpeg-decoder`, `zune-jpeg`, `turbojpeg`, and FFmpeg `mjpeg`. **All output RG24
  only.** FFmpeg's `new_nv12_zero_copy` exists but is not registered.
- None of them do a luma-only decode, use TJ scaling factors, or crop.
- `TurbojpegDecoder` already decodes straight into the pooled buffer.
- `decode_to_rg24_for_format` hard-codes `MjpegDecoder` (jpeg-decoder) for JPEG input
  (`runtime_codec.rs` L332). The runtime selector prefers turbojpeg, but registry `lookup` falls back
  to alphabetical order, where ffmpeg wins.
- Hardware decode exists only through FFmpeg: H.264/H.265 via `v4l2m2m` and `v4l2request` with DRM PRIME
  dmabuf out (`ffmpeg/decoder/drm_prime.rs`). There is no VA-API, NVDEC/NVJPG, MPP or RGA code.
- Luma extractors exist only for raw input: `YuyvToLumaDecoder` and `Nv12ToLumaDecoder`.

---

## 2. Per-target findings

### 2.1 Raspberry Pi 5 / CM5 (BCM2712, PiSP)

Hardware [D]:

- **PiSP**: the Front End writes to memory and the Back End is a memory-to-memory ISP.
  - The Back End has two processed outputs (Output0 and Output1), each with its own crop, downscaler,
    resampler and format stage, fed from the same input frame. libcamera's `rpi/pisp` pipeline
    handler exposes up to 2 processed streams plus 1 raw stream.
  - libcamera completes both output buffers in the **same Request**, so they share the sensor
    timestamp and sequence by construction.
- **No hardware JPEG or H.264 decode.** Pi 5 only has an HEVC decoder (stateless V4L2, `rpi-hevc-dec`).
  USB MJPEG on a Pi 5 is therefore always a CPU decode.

Your questions, answered on `helios` [M-CM5] unless marked:

- **Y8 or grayscale stream?**
  - No direct Y8 from the ISP for a colour sensor. Requesting `R8` validates as *Adjusted* to
    `SBGGR16` raw: libcamera treats R8 as a mono-sensor raw format.
  - Use plane 0 of NV12 or YUV420 instead. `YUV420` single stream validates as-is, stride 1280 at
    1280×800. The Y plane is zero-copy.
  - Styx needs a `luma()` view and must stop routing GREY to the Raw role.
  - The ISP also writes chroma (+50% DRAM writes). That costs bandwidth, not CPU.
- **Two outputs from one sensor?** Yes, confirmed.

  | Config | Result | Strides |
  |---|---|---|
  | NV12 1280×800 + NV12 640×400 | Adjusted (colour space only) | 1280 / 640 |
  | NV12 1280×800 + NV12 320×200 | Adjusted | 1280 / 320 |
  | NV12 1280×800 + NV12 160×100 | Adjusted | 1280 / **192** |
  | YUV420 full + YUV420 ½ | Adjusted | 1280 / 640 |
  | NV12 ½ + NV12 full (order swapped) | Adjusted, sizes kept | 640 / 1280 |
  | NV12 full + NV12 ½ + Raw | Adjusted (raw = `BGGR_PISP_COMP1`) | 1280 / 640 / 1280 |
  | R8 full + R8 ½ | **Invalid** ("Invalid number of streams") | – |

  Capturing NV12 full + ½ for 150 frames, **every Request carried both buffers with identical
  timestamps and sequence numbers**. Median frame interval was 33.3 ms (the OV9782's
  `FrameDurationLimits` minimum is 33.3 ms, so 30 fps max). Completion arrived 8.9 ms after
  `SensorTimestamp`. Styx has the dual-stream plumbing (TDN) but forces equal sizes and emits only
  one buffer.
- **Hardware crop or ROI?** Yes.
  - `ScalerCrop` and the Pi vendor control `ScalerCrops` (per output) are both exposed.
  - Setting a centre-quarter `ScalerCrop` showed up in the request metadata **4 frames later**
    (~133 ms at 30 fps).
  - Styx drops all Rectangle controls today.
- **Stride:** PiSP output strides are 64-byte aligned (160 px → 192 B). Keep reading
  `StreamConfiguration::stride`.
- **Memory:** see [TL;DR #1](#tldr). For CPU consumers, Styx should allocate cached dma-heap buffers
  and pass them to libcamera as `FrameBuffer`s (tested and working with both `linux,cma` and
  `system` heaps). That makes `DMA_BUF_IOCTL_SYNC` mandatory.

### 2.2 RK3588 (Orange Pi 5, Rock 5)

| Block | Linux access | Build/run requirements [D] |
|---|---|---|
| **RGA3 ×2 + RGA2** (scale, convert, crop, rotate) | `librga` (`im2d` API) → `/dev/rga` (vendor kernel driver); dmabuf import via `importbuffer_fd` | BSP kernel (5.10/6.1) for RGA3. Mainline has only the RGA2 V4L2 M2M driver (`rockchip-rga`); an RGA3 upstream driver was *in progress*, *verify*. librga license: Apache-2.0 in the airockchip repo (*verify* before shipping). RGA2 needs buffers below 4 GB (use the dma32 heap). Scale range 1/16–16×. Y400 output: *verify* per core. |
| **MPP** (VDPU/JPEG decoder, H.264/H.265/VP9/AV1) | `librockchip_mpp` → `/dev/mpp_service`; outputs DRM-PRIME dmabuf NV12/NV16 | BSP kernel. MPP is Apache-2.0 / MIT (*verify*). Mainline has H.264/HEVC via `rkvdec2` on recent kernels, but no MJPEG decode (*verify*). The pragmatic route is the `ffmpeg-rockchip` fork (`mjpeg_rkmpp`, `h264_rkmpp`, RGA filters), which plugs into Styx's existing DRM PRIME path. |
| **rkisp (ISP30)**: mainpath + selfpath, two scaled outputs | V4L2 + media controller; 3A via `rkaiq` | BSP only. `rkaiq` is a closed binary with a Rockchip license. libcamera's `rkisp1` handler covers RK3399/i.MX8MP, not RK3588 BSP. **High effort**, and irrelevant for USB cameras, which are the common case on Orange Pi. |

### 2.3 NVIDIA Jetson Orin

| Block | Linux access | Notes [D] |
|---|---|---|
| **VIC** (scale, convert, crop) | `NvBufSurfTransform` (Multimedia API / nvbufsurface), or VPI with the VIC backend. **Not** a standard V4L2 M2M node. | Output layout can be `NVBUF_LAYOUT_PITCH` (CPU-friendly) instead of block-linear. |
| **NVJPG** | `NvJPEGDecoder` (Multimedia API, libjpeg-like) → `decodeToFd` gives an NvBufSurface dmabuf. `nvv4l2decoder mjpeg=1` uses NVDEC instead. | Orin Nano: NVJPG count and presence need checking per SKU (*verify*). Orin NX/AGX have it. |
| **CPU readability** | `NvBufSurfaceMap` + **`NvBufSurfaceSyncForCpu`** before reading. Mappings are cached, and the explicit sync is required. | Pitch-linear + map + sync gives a normal cached CPU read. Block-linear surfaces must go through VIC to pitch first (cheap, but one more hop). |

Licensing: the Multimedia API and VPI are proprietary NVIDIA (L4T license). The runtime libraries
ship with JetPack. **`dlopen` them at runtime** behind a feature so Styx never links NVIDIA code at
build time and CI doesn't need JetPack.

### 2.4 x86 mini-PCs (Intel N100, AMD APUs)

- **Intel N100 (Gen12 Xe-LP)**:
  - The iHD VA-API driver exposes `VAProfileJPEGBaseline/VLD` plus `VideoProc` [D].
  - Surfaces are tiled. `vaDeriveImage` on tiled surfaces either fails or copies. Use VPP to convert
    to a *linear* NV12 or Y800 surface, then `vaExportSurfaceHandle` → dmabuf.
  - The iGPU shares the LLC, so cached CPU reads of the exported buffer are cheap [D].
- **AMD APUs**:
  - VCN has JPEG decode through radeonsi VA-API (confirmed present on the local RX 6800 XT [M]).
  - APU mappings are typically write-combined, and **CPU reads of WC memory are ~10× slower** [D].
    This is the same trap you hit on the Pi GPU, so it needs a GPU-side copy to a cacheable buffer
    or an SDMA blit.
- **Local measurement** (RX 6800 XT dGPU, not an N100, see [M3](#m3-va-api-mjpeg-decode-amd-vcn-rx-6800-xt-via-ffmpeg)):
  - Decode-only CPU cost is 0.26–0.39 ms/frame for 4:2:0 720p/1080p, but serial wall latency is
    1.2–2.7 ms.
  - For **4:2:2** (the typical UVC camera), the surface comes back as 4:2:2 and the NV12 conversion
    lands on the CPU: 3.5–7.9 ms CPU/frame, **worse than libjpeg-turbo**.
- **Is VA-API decode to a CPU-readable Y8 faster than libjpeg-turbo?** On a fast desktop core, no:
  libjpeg-turbo GREY is 1.1–2.5 ms at 720p and 2.4–5.9 ms at 1080p [M].
  - On an N100, libjpeg-turbo is roughly 1.7–2.2× slower [E]: about 2–5.5 ms at 720p and 4–13 ms
    at 1080p.
  - QuickSync then saves most of that CPU time, **provided we map only the Y plane of a linear
    surface**. Expect a latency increase of ~1–2 ms [E from M3].

### 2.5 Software MJPEG fast paths (all targets)

The table is **[M-CM5]**: libjpeg-turbo 3.1.0 on the CM5, Cortex-A76 @ 2.4 GHz, `performance`
governor. The x86 columns are [M] for comparison. Full tables are in [CM5-1](#cm5-1-mjpeg-decode-on-cortex-a76)
and [M1](#m1-libjpeg-turbo-decode-modes-msframe).

| ms/frame | CM5, real OV9782 800p 4:2:2 (73 KiB) | CM5, synthetic 720p 4:2:2 (235 KiB) | CM5, synthetic 1080p 4:2:2 (532 KiB) | x86, synthetic 720p 4:2:2 |
|---|---:|---:|---:|---:|
| Full decode → planar YUV | 3.16 | 6.28 | 14.22 | 2.96 |
| Full decode → RGB | 4.49 | 7.48 | 16.68 | 3.31 |
| **Luma-only (TJPF_GRAY)** | **2.36** | **5.02** | **11.19** | **2.48** |
| GRAY at ½ / ¼ / ⅛ (DCT scaling) | 1.96 / 1.74 / 1.37 | 4.39 / 4.11 / 3.53 | 9.75 / 9.12 / 7.83 | 2.34 / 2.19 / 1.91 |
| **GRAY full + ½ box (software pyramid)** | **2.43** | **5.08** | **11.36** | 2.63 |
| GRAY full + ½ + ¼ box | 2.45 | 5.10 | 11.46 | 2.67 |
| GRAY full + GRAY ¼ (two decodes) | 4.10 | 9.12 | 20.46 | 4.69 |
| GRAY, crop centre ¼ area / centre 1/16 area | 1.22 / 0.88 | 3.11 / 2.32 | 6.71 / 5.06 | 1.63 / 1.25 |
| GRAY, crop to bottom half | 1.78 | 4.23 | 9.39 | 2.18 |
| **Styx today: `jpeg-decoder` → RGB (Rust)** | **20.42** | **24.55** | **55.66** | 5.56 |
| `zune-jpeg` → RGB / → Luma (Rust) | 6.38 / 3.74 | 9.67 / 6.56 | 22.02 / 15.00 | 3.78 / 2.91 |

What this means:

- **Entropy decoding sets the floor.** A ⅛ decode (DC-only IDCT) still costs ~65–75% of GRAY full.
  Every row above the lowest needed row must be Huffman-decoded, which is why "bottom half" barely
  helps while "centre" does: decoding stops at the ROI's last row.
- **Luma-only** skips IDCT, upsampling and colour conversion for Cb and Cr. It saves 16–22% on 4:2:2
  and ~10% on 4:2:0.
- **Decode once, box-filter in software.** One full decode plus a 2×2 box filter is the right way to
  get a pyramid from MJPEG. On the CM5 the box filter (auto-vectorised C, `-O3 -mcpu=cortex-a76`)
  costs +0.06 ms at 720p/800p and +0.16–0.19 ms at 1080p [M-CM5]. A DCT-scaled second decode costs
  another 65–85%.
- **Versus the Styx default:** on the CM5, turbojpeg GRAY is **4.9–9.0× faster** than `jpeg-decoder`
  RGB [M-CM5]. For example, 2.36 vs 20.42 ms on real 800p frames, and 11.2 vs 55.7 ms at 1080p. It
  also removes Eidos' RGB→Y pass. `jpeg-decoder` is disproportionately slow on ARM: 4.9× slower
  than turbojpeg GRAY on the CM5 versus 2.2× on x86, at 720p 4:2:2.
- **Restart markers:** if the camera emits restart markers (DRI), slices can be decoded in parallel
  on 4 A76 cores. Many UVC cameras don't. Check per camera model. Pipelining frames across cores
  improves throughput, not latency.
- **CM5 vs x86 [M-CM5]:** libjpeg-turbo on the A76 is only **1.9–2.2× slower** than on Zen 3. My
  earlier estimate of 2.5–3.5× was pessimistic.
  - The premise still holds: at 720p–1080p, MJPEG decode on the CM5 costs **2–11 ms with the fast
    path and 16–56 ms today**, versus ~1 ms for Eidos.
  - A high-bitrate 1080p 4:2:2 stream (11.2 ms GRAY) uses a third of a 30 fps frame budget on one
    core even after the fix. That is where hardware decode, or parallel decode with restart markers,
    would start to matter.

### 2.6 Generic V4L2 M2M

Standard V4L2 M2M can cover [D]:

- Converters and scalers: mainline `rockchip-rga` (RGA2 only), NXP `imx-pxp`, the i.MX8 ISI M2M, and
  some Allwinner devices.
- Stateful JPEG decoders: NXP `mxc-jpeg`, and Qualcomm/Venus on some SoCs.
- Stateless decode through the request API: H.264 and HEVC only. There is **no stateless JPEG uAPI**.

It does **not** cover our listed targets as usually shipped:

- **Pi 5:** PiSP BE is M2M but needs `libpisp` configuration and media-controller topology, not a
  plain `S_FMT` scaler.
- **Jetson:** nvbufsurface, not V4L2 converters.
- **RK3588 BSP:** librga and MPP.
- **x86:** VA-API.

Verdict: build a small generic M2M converter as a *secondary* backend, useful for NXP and mainline
Rockchip. Don't expect it to replace vendor code. Styx already gets V4L2 M2M decode through FFmpeg
for H.264/H.265.

---

## 3. Summary table

Effort: S ≈ ≤3 days, M ≈ 1–2 weeks, L ≈ 3+ weeks (including on-device testing).
"Gain" is CPU time saved per 720p frame on that target unless stated.

| Target | Hardware block | Linux API | Styx status | Effort | Expected gain |
|---|---|---|---|---|---|
| Pi 5 | Cached dma-heap buffers + `DMA_BUF_IOCTL_SYNC` | dma-heap + libcamera `FrameBuffer` import | **Missing** (uncached `FrameBufferAllocator`, no sync) | S–M | Y-plane scalar read 10.2 → 0.69 ms; NEON 0.59 → 0.04 ms; sync ~55 µs/MB [M-CM5] |
| All | libjpeg-turbo GREY + SW box pyramid | turbojpeg (already linked) | **Missing** (RG24 only) | S | CM5: 4.9–9.0× vs today, −18 to −44 ms/frame at 800p–1080p [M-CM5] |
| All | libjpeg-turbo crop (ROI) | `tj3SetCroppingRegion` | Missing | S | 38–62% for centre ROIs, 16–24% for bottom half [M-CM5] |
| All | Y-plane view of NV12/I420 | none (view) | Partial (planes exposed, no luma helper) | S | Avoids `Nv12ToLuma` copy (~0.05–0.2 ms) [E] |
| Pi 5 | PiSP BE dual output (pyramid) | libcamera 2 processed streams | **Partial** (TDN 2-stream, same size, 1 buffer emitted) | M | Pyramid at 0 CPU, frame-matched (150/150) [M-CM5]; saves the SW box (~0.06 ms) |
| Pi 5 | ScalerCrop / ScalerCrops | libcamera controls | Missing (Rectangle skipped) | S–M | ISP-side, applies after 4 frames [M-CM5] |
| Pi 5 | Direct R8 output | libcamera | **Not available** for colour sensors (rewritten to raw) [M-CM5] | n/a | Use the NV12/YUV420 plane-0 view |
| Pi 5 | HW JPEG decode | none (no block) | n/a | n/a | n/a |
| RK3588 | MPP JPEG/H.264/H.265 decode | librockchip_mpp or ffmpeg-rockchip | Missing (FFmpeg DRM PRIME path reusable) | M (via ffmpeg-rockchip) / L (native) | ~4–10 ms/frame for 720p–1080p MJPEG on A76 [E] |
| RK3588 | RGA3 scale, convert, crop | librga im2d, dmabuf | Missing | M | Pyramid + NV16→Y8 at ~0 CPU; latency +~1 ms [E] |
| RK3588 | rkisp mainpath/selfpath | V4L2 + MC + rkaiq | Missing | L | Only for CSI sensors |
| Jetson Orin | NVJPG decode | NvJPEGDecoder (MMAPI) | Missing | M–L | ~4–10 ms/frame for 720p–1080p on A78AE [E] |
| Jetson Orin | VIC scale, convert, crop | NvBufSurfTransform / VPI | Missing | M | Pyramid and pitch-linear Y8 at ~0 CPU |
| Intel N100 | QuickSync JPEG + VPP | VA-API | Missing | M | ~2–5 ms/frame at 720p [E]; latency +1–2 ms |
| AMD | VCN JPEG | VA-API | Missing | M | CPU 0.26–0.4 ms vs 1.1–2.5 ms SW on fast cores [M]; risky on APUs (WC reads) |
| Generic | V4L2 M2M scaler/decoder | V4L2 | Missing (FFmpeg for H.26x only) | M | Covers NXP / mainline RGA2, not primary targets |
| Correctness | GREY via V4L2; GREY→Raw role on libcamera | n/a | **Bugs** [C] | S | Makes Y8 capture work at all |
| Correctness | DMA_BUF_IOCTL_SYNC | dma-buf | Missing | S | Required with cached buffers; stale reads seen without it [M-CM5] |

---

## 4. Ranked plan

0. **Cached, synced libcamera buffers (S–M, Pi 5 CSI, and HeliOS today).**
   - Why first: every CSI frame Eidos sees on the CM5 today is in uncached memory, where scalar
     per-pixel reads are ~15× slower than heap memory [M-CM5]. Eidos' 1 ms figure only holds if its
     input is cached.
   - Build:
     - Allocate from `/dev/dma_heap/linux,cma` (or `system`) and create libcamera `FrameBuffer`s from
       those fds, instead of `FrameBufferAllocator` (tested working).
     - Add `DMA_BUF_IOCTL_SYNC` START on first `plane_data()` and END on drop in `LibcameraBacking`
       (~55 µs/MB).
   - Stopgap if this slips: NEON-copy the Y plane out (0.59–0.67 ms/MB) instead of reading it in place.
1. **Software MJPEG → Y8 path + SW pyramid + ROI crop (S, all targets).**
   - Why next: it's measured on the target (4.9–9.0× on the CM5), the largest win per unit of effort,
     needs no new dependencies, and is the *only* option on Pi 5 USB cameras.
   - Build:
     - A `TurbojpegLumaDecoder` (MJPG→GREY) that writes into 64-B-aligned pooled buffers.
     - An optional in-place 2×2/4×4 box companion (NEON/SSE2 via `std::arch`, scalar fallback).
     - An optional `tj3SetCroppingRegion`.
   - Also fix the registry default so MJPEG never falls back to `jpeg-decoder` when turbojpeg is
     available.
2. **Correctness fixes that block Y8 at all (S).**
   - V4L2 `GREY`/`R8` stride (and the NV12 single-plane layout).
   - libcamera `GREY`→Raw role mapping.
   - `DMA_BUF_IOCTL_SYNC` for `SharedFdBacking` and V4L2 exports (libcamera is covered by #0).
   - Propagate the libcamera sequence number.
3. **libcamera dual-stream pyramid + Y-plane view + ScalerCrop (M, Pi 5 CSI).**
   - This makes Pi CSI cameras effectively free: zero decode, zero-copy Y8, and a hardware ½ or ¼
     image with the same timestamp. All of this is confirmed on `helios`.
   - It reuses the TDN two-stream code and needs the `FrameLease` companion API (Section 5).
4. **RK3588 MPP MJPEG decode via `ffmpeg-rockchip` + RGA pyramid (M).**
   - Weakest-CPU-to-best-hardware ratio among the ARM targets, and it reuses Styx's DRM PRIME code.
   - The build depends on a forked FFmpeg, so keep it behind a feature and a runtime probe.
5. **VA-API MJPEG decode (M), Intel first.**
   - Only worth it with a linear VPP output and a direct Y-plane map, not "download to NV12".
   - AMD APUs are lower priority because of write-combined CPU reads.
6. **Jetson NVJPG + VIC (M–L).** `dlopen` NVIDIA libraries, use pitch-linear surfaces with explicit
   CPU sync.
7. **Generic V4L2 M2M converter (M), rkisp (L).** Only on demand.

**Where this departs from the original guess:**

- (a) "Hardware or cheaper software MJPEG decode" splits into two very different items. The *cheap
  software* part is #1 and is the clear first step. *Hardware* decode moves to #4–#6: it needs
  per-vendor code, and it only pays off with careful zero-copy mapping (the local measurement shows a
  naive download path can cost as much CPU as software for 4:2:2).
- (b) Pyramids: for MJPEG sources, the pyramid should be *software* (one decode + box filter, <0.2 ms)
  and ships with #1. The *hardware* dual-output pyramid matters for CSI sensors (#3), where it's
  cheap to add because the dual-stream plumbing already exists.

---

## 5. API sketch

Design goals:

- Don't grow `FrameLease` for the common case (one `Option<Box<…>>`).
- Keep companions as real `FrameLease`s, so every existing view, validation and export helper works
  on them.
- Keep backings shared: for example, one libcamera Request is requeued when *both* the primary and
  the companion are dropped.

### 5.1 Companion frames on `FrameLease`

```rust
// crates/core/src/buffer/frame.rs

/// Why a companion exists relative to its primary frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompanionKind {
    /// Downscaled by 2^level in each axis (level 1 = ½, 2 = ¼).
    Pyramid { level: u8 },
    /// A cropped region of the primary, at the primary's scale.
    Roi { id: u8 },
}

#[derive(Clone, Copy, Debug)]
pub struct CompanionInfo {
    pub kind: CompanionKind,
    /// Region of the primary this companion covers, in primary pixel coordinates.
    pub source_rect: Rect,
    /// How it was produced (lets Eidos tune thresholds for box vs ISP-resampled images).
    pub producer: CompanionProducer, // Isp | Scaler2d | CpuBox | DctScaled
}

pub struct FrameLease {
    meta: FrameMeta,
    buffers: SmallVec<[BufferLease; 3]>,
    layouts: SmallVec<[PlaneLayout; 3]>,
    external: Option<Arc<dyn ExternalBacking>>,
    companions: Option<Box<SmallVec<[(CompanionInfo, FrameLease); 2]>>>, // new
}

impl FrameLease {
    pub fn companions(&self) -> impl Iterator<Item = (&CompanionInfo, &FrameLease)>;
    pub fn companion(&self, kind: CompanionKind) -> Option<&FrameLease>;
    pub fn pyramid_level(&self, level: u8) -> Option<&FrameLease> {
        if level == 0 { Some(self) } else { self.companion(CompanionKind::Pyramid { level }) }
    }
    /// Attach at build time; enforces equal timestamps and a non-recursive companion.
    pub fn with_companion(self, info: CompanionInfo, frame: FrameLease)
        -> Result<Self, FrameValidationError>;

    /// Zero-copy Y8 view for GREY, R8, NV12, NV21, I420, YU12, YV12 (plane 0).
    /// Returns Err for packed YUYV/RGB, which need a conversion step instead.
    pub fn luma(&self) -> Result<VisibleRows<'_>, FrameValidationError>;
}
```

Notes:

- `into_parts`, `materialize_owned` and `export_*` must decide whether to carry companions:
  - Materialize: yes, recursively.
  - Export: export only the primary, and add `export_companion(kind)`.
- Frame matching comes from the producer: both from one Request, or one decode. The companion's
  `FrameMeta.timestamp` equals the primary's, and `with_companion` checks it.
- For libcamera, `LibcameraBacking` becomes `Arc<RequestHold>` shared by two `ExternalBacking`s (one
  per stream). The Request is requeued when the last holder drops.
- Add `BackendFrameMeta::Libcamera { sequence, sensor_timestamp }` alongside `V4l2`.
- Add `pub struct Rect { x, y, width, height: u32 }` to `format.rs`.
- Add `FourCc::Y8` as an alias constant for `GREY` if that's more readable for Eidos.

### 5.2 Consumer-declared requirements

(The proposal as written; what was built is `styx::planner::FrameRequest`, built with
`Frames::...`: see [frame-planning.md](frame-planning.md).)

```rust
// crates/core (so Eidos can depend on the `framelease` feature only)

#[derive(Clone, Debug, Default)]
pub struct FrameRequirements {
    /// Preferred output formats in order, e.g. [GREY] or [GREY, NV12] ("any format with a luma view").
    pub formats: SmallVec<[FourCc; 4]>,
    /// Accept NV12/I420 and read plane 0 when GREY isn't native (zero-copy Y view).
    pub accept_luma_view: bool,
    /// Row stride alignment in bytes (Eidos: 64). Also aligns plane base addresses.
    pub stride_alignment: Option<usize>,
    pub pyramid: Option<PyramidRequest>,
    pub roi: Option<Rect>,               // initial ROI in sensor or frame coordinates
    pub residency: ResidencyRequirement, // HostReadable (Eidos) | Exportable | Any
}

#[derive(Clone, Debug)]
pub struct PyramidRequest {
    pub levels: SmallVec<[u8; 2]>,       // [1] = ½, [2] = ¼, [1, 2] = both
    pub source: PyramidSource,
}

pub enum PyramidSource {
    HardwareOnly,     // fail planning if no ISP/scaler can produce it
    PreferHardware,   // fall back to CPU box filter (default)
    Cpu,
}
```

Where it plugs in:

- **Selection:** `CameraRequest::requirements(FrameRequirements)`.
  - Changes format scoring so GREY/R8, then NV12/I420 (with `accept_luma_view`), rank above RGB.
  - MJPG ranks above YUYV only when an MJPG→GREY decoder is available and USB bandwidth needs it.
- **Planning:** extend `StyxPathRequest` / `explain_styx_path` to produce a `FramePreparationPlan`.
  - It is a short list of steps, each tagged `ZeroCopyView | HardwareFixedFunction | CpuSimd | CpuCopy`,
    with an estimated cost.
  - Examples:
    - PiSP: `[Isp{out0: NV12 full, out1: NV12 ½}, LumaView, LumaView]`
    - USB MJPEG on Pi: `[TurboJpegGray{align:64}, CpuBox2]`
    - RK3588 MJPEG: `[MppDecode→NV16 dmabuf, Rga{Y8 full + Y8 ½}]`
- **Execution:** `MediaPipelineBuilder::prepare_for(requirements)` installs the planned decoder or
  preparer in the existing decoder slot. It needs no new pipeline stage type: a `Codec` that emits a
  `FrameLease` with companions is enough.
- **Runtime ROI:** `CaptureHandle::set_roi(Option<Rect>)`, mapped per backend:
  - libcamera: ScalerCrop / rpi::ScalerCrops
  - RGA/VIC: crop rectangle
  - turbojpeg: `tj3SetCroppingRegion`, MCU-aligned (the actual rect is reported in `CompanionInfo.source_rect`)
  - V4L2: `S_SELECTION` if supported, else CPU crop view

---

## 6. Risks

| Risk | Detail | Mitigation |
|---|---|---|
| Driver availability | RGA3, MPP and rkisp need Rockchip BSP kernels. Mainline RK3588 covers H.264/HEVC decode and RGA2 only (*verify* at ship time). Jetson needs JetPack. VA-API JPEG needs iHD/radeonsi with JPEG enabled; some distros build Mesa without some codecs. Pi 5 has no JPEG block at all. | Runtime probe per backend (as the FFmpeg v4l2m2m probe already does). Always keep the software path. Report the chosen path in `FramePreparationPlan`. |
| Licensing | NVIDIA MMAPI and VPI are proprietary. `rkaiq` is a closed binary. librga and MPP are open (Apache-2.0/MIT per upstream, *verify*). `ffmpeg-rockchip` is LGPL/GPL depending on build. | `dlopen` NVIDIA and Rockchip libraries behind features. Never link them in default builds. Document which features pull in GPL FFmpeg. |
| Sync and latency | Each hardware hop adds a submit + fence wait. Measured VA-API wall latency was 1.2–2.7 ms/frame serial vs 1.1–2.4 ms software GRAY [M]. Pipelining helps throughput, not latency. ScalerCrop changes apply a few frames late. | Choose hardware only when the CPU is the bottleneck. Keep ≥2 frames in flight. Tag ROI companions with their actual `source_rect` so Eidos can tell when a crop took effect. |
| Cache coherency | Confirmed on the CM5 [M-CM5]: the PiSP is not IO-coherent. Today's libcamera buffers avoid stale data only by being uncached, which makes them ~15× slower for scalar reads. With cached dma-heap buffers and no sync, repeated reads of one completed buffer returned different checksums, and 38/145 frames on the `system` heap were stale until `SYNC_START`. Jetson needs `NvBufSurfaceSyncForCpu`. AMD APU surfaces are write-combined (slow reads). Intel iGPU is LLC-coherent. | Cached heap buffers + `SYNC_START(READ)` on first `plane_data()` and `SYNC_END` on drop. Measured cost is 32 + 23 µs per MB on `linux,cma` and 52 + 35 µs on `system`. Never read uncached or WC memory with scalar loops; NEON-copy once if a buffer must stay uncached. |
| Chroma subsampling | UVC MJPEG is often 4:2:2. Hardware decoders then emit NV16/422 surfaces, and generic "to NV12" paths push the conversion onto the CPU [M]. | Consume the Y plane directly regardless of chroma format. Don't convert. |
| Tiled layouts | Intel VA surfaces and Jetson block-linear surfaces aren't CPU-friendly. | Force a linear or pitch-linear output (VPP / VIC) as part of the plan. |
| Pool alignment | `BufferPool` returns `Vec<u8>` with only malloc alignment. | Over-allocate by 63 bytes and use `PlaneLayout.offset` to align the base. No new buffer type needed. |

---

## Measurements

Host: AMD Ryzen 9 5900X (Zen 3), Fedora (Nobara) kernel 7.2.3, libjpeg-turbo 3.1.3,
zune-jpeg 0.5.15, jpeg-decoder 0.3.2, FFmpeg + Mesa 26.2.3 radeonsi (RX 6800 XT VCN).
Single core pinned (`taskset -c 3`). Best of 5 runs over 30 frames × 4 iterations.

**Test streams:** synthetic but camera-like. A textured background, a high-contrast cellular-automaton
block pattern (marker-like edges), and per-frame temporal noise (σ≈10), encoded by FFmpeg `mjpeg`.
Two quality levels bracket typical UVC bitrates: 63–70 KiB (q6) and 178–235 KiB (q3) per 720p frame.
Real camera streams should be re-measured. Quantisation tables and entropy statistics differ by vendor.

**Scratch sources** are in the session scratchpad, not the repo: `tjbench.c`, `tjcrop.c`,
`rsbench/`, and the libcamera probes `lcprobe.cpp` / `lcmem.cpp`. The decode ones can become a
criterion bench under `crates/codec/benches` so they run on the Orange Pi 5 and Jetson too.

### M1. libjpeg-turbo decode modes (ms/frame)

| Stream | YUV full | RGB full | GRAY full | GRAY fastdct | GRAY ½ | GRAY ¼ | GRAY ⅛ | GRAY + box½ | GRAY + box½ + box¼ | GRAY + GRAY¼ |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 720p 4:2:0 q6 (63 KiB) | 1.18 | 1.57 | 1.06 | 1.00 | 0.91 | 0.78 | 0.65 | 1.21 | 1.27 | 1.85 |
| 720p 4:2:2 q6 (70 KiB) | 1.51 | 1.89 | 1.18 | 1.12 | 1.02 | 0.91 | 0.79 | 1.36 | 1.41 | 2.14 |
| 720p 4:2:0 q3 (178 KiB) | 2.20 | 2.58 | 2.01 | 1.95 | 1.87 | 1.78 | 1.48 | 2.21 | 2.23 | 3.81 |
| 720p 4:2:2 q3 (235 KiB) | 2.96 | 3.31 | 2.48 | 2.44 | 2.34 | 2.19 | 1.91 | 2.63 | 2.67 | 4.69 |
| 1080p 4:2:0 q6 (141 KiB) | 2.69 | 3.52 | 2.37 | 2.21 | 2.00 | 1.72 | 1.43 | 2.71 | 2.79 | 4.09 |
| 1080p 4:2:2 q6 (161 KiB) | 3.42 | 4.12 | 2.66 | 2.53 | 2.31 | 2.02 | 1.73 | 3.01 | 3.13 | 4.74 |
| 1080p 4:2:0 q3 (399 KiB) | 4.89 | 5.63 | 4.44 | 4.29 | 4.03 | 3.92 | 3.22 | 4.73 | 4.92 | 8.38 |
| 1080p 4:2:2 q3 (532 KiB) | 6.82 | 7.73 | 5.88 | 5.74 | 5.53 | 4.98 | 4.36 | 6.32 | 6.36 | 11.19 |

The box filter is naive scalar C (`-O2`). A SIMD version should be several times cheaper.
`tj3Init`+`tj3Destroy` costs 0.19 µs, so the per-frame `Decompressor::new()` in `mjpeg_turbojpeg.rs`
is not a performance problem.

### M2. Rust decoders used by Styx (ms/frame, single thread)

| Stream | zune RGB | zune Luma | jpeg-decoder RGB (Styx default) |
|---|---:|---:|---:|
| 720p 4:2:2 q3 | 3.78 | 2.91 | 5.56 |
| 720p 4:2:2 q6 | 2.16 | 1.48 | 4.00 |
| 1080p 4:2:2 q3 | 8.70 | 6.70 | 12.84 |
| 1080p 4:2:0 q6 | 4.48 | 3.23 | 7.32 |

`docs/performance.md` reports 13.7 ms for jpeg-decoder at 720p on a different machine or content.
The ratio against turbojpeg GRAY is what matters here.

### M3. VA-API MJPEG decode (AMD VCN, RX 6800 XT, via FFmpeg)

300 frames each. There is one frame in flight (FFmpeg serial decode), so wall time per frame is
latency. CPU time is user + sys and includes FFmpeg demux and muxer overhead.

| Stream | GPU-only: CPU / wall | → CPU NV12: CPU / wall | → CPU GRAY: CPU / wall |
|---|---|---|---|
| 720p 4:2:0 q6 | 0.26 / 1.20 ms | 0.65 / 1.97 ms | 0.85 / 1.91 ms |
| 1080p 4:2:0 q6 | 0.39 / 2.70 ms | 1.51 / 4.30 ms | 2.42 / 4.35 ms |
| 720p 4:2:2 q3 | 0.53 / 1.79 ms | 3.46 / 2.54 ms | 2.24 / 2.62 ms |
| 1080p 4:2:2 q3 | 1.08 / 4.04 ms | 7.91 / 5.73 ms | 4.91 / 5.47 ms |

For 4:2:2 input, the surface download is followed by a CPU swscale conversion, which dominates.
A Styx implementation that maps the Y plane directly would sit near the GPU-only column plus a
~1 MB (720p) or ~2 MB (1080p) read of the Y plane.

## On-device measurements (helios CM5)

**Device:** Raspberry Pi CM5, 4× Cortex-A76 @ 2.4 GHz (`performance` governor, 38–44 °C), 2 GB,
HeliOS v2026.2.0 (Buildroot, glibc 2.41), kernel 6.12.47-v8-16k, libcamera 0.6.0+rpt20251202,
libturbojpeg 3.1.0. Sensor: OV9782 (1280×800 colour global shutter) on CSI.

**Setup:**
- `helios-peripherals` was stopped during the camera tests and restarted afterwards (confirmed
  active, camera re-acquired).
- Binaries were cross-compiled with the HeliOS Buildroot toolchain and run from `/tmp/styx-probe`.
- There was no USB MJPEG camera. For "real" frames, 30 OV9782 NV12 frames were captured on the
  device and MJPEG-encoded with FFmpeg (4:2:2 and 4:2:0, q3/q6: 43–73 KiB/frame).

### CM5-1. MJPEG decode on Cortex-A76

libjpeg-turbo, ms/frame, best of 5 × 2 iterations × 30 frames:

| Stream | YUV full | RGB full | GRAY full | GRAY fastdct | GRAY ½ | GRAY ¼ | GRAY ⅛ | GRAY + box½ | GRAY + box½ + box¼ | GRAY + GRAY¼ |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| real 800p 4:2:0 q6 (43 KiB) | 2.20 | 3.63 | 1.80 | 1.66 | 1.44 | 1.24 | 0.90 | 1.87 | 1.89 | 3.03 |
| real 800p 4:2:2 q6 (47 KiB) | 2.70 | 4.03 | 1.95 | 1.81 | 1.59 | 1.38 | 1.05 | 2.02 | 2.04 | 3.33 |
| real 800p 4:2:0 q3 (68 KiB) | 2.60 | 4.03 | 2.17 | 2.00 | 1.78 | 1.56 | 1.19 | 2.24 | 2.26 | 3.74 |
| real 800p 4:2:2 q3 (73 KiB) | 3.16 | 4.49 | 2.36 | 2.19 | 1.96 | 1.74 | 1.37 | 2.43 | 2.45 | 4.10 |
| 720p 4:2:0 q6 (63 KiB) | 2.60 | 3.89 | 2.26 | 1.94 | 1.75 | 1.49 | 1.14 | 2.32 | 2.34 | 3.75 |
| 720p 4:2:2 q6 (70 KiB) | 3.26 | 4.47 | 2.48 | 2.16 | 1.96 | 1.70 | 1.36 | 2.55 | 2.56 | 4.19 |
| 720p 4:2:0 q3 (178 KiB) | 4.65 | 5.95 | 4.19 | 3.70 | 3.57 | 3.30 | 2.69 | 4.26 | 4.27 | 7.48 |
| 720p 4:2:2 q3 (235 KiB) | 6.28 | 7.48 | 5.02 | 4.54 | 4.39 | 4.11 | 3.53 | 5.08 | 5.10 | 9.12 |
| 1080p 4:2:0 q6 (141 KiB) | 5.99 | 8.76 | 5.10 | 4.35 | 3.90 | 3.33 | 2.56 | 5.29 | 5.39 | 8.49 |
| 1080p 4:2:2 q6 (161 KiB) | 7.50 | 9.99 | 5.55 | 4.85 | 4.40 | 3.83 | 3.07 | 5.74 | 5.85 | 9.45 |
| 1080p 4:2:0 q3 (399 KiB) | 10.51 | 13.22 | 9.33 | 8.20 | 7.88 | 7.25 | 5.91 | 9.52 | 9.64 | 16.70 |
| 1080p 4:2:2 q3 (532 KiB) | 14.22 | 16.68 | 11.19 | 10.09 | 9.75 | 9.12 | 7.83 | 11.36 | 11.46 | 20.46 |

Crop (`tj3SetCroppingRegion`, GRAY), ms:

| Stream | full | centre ¼ area | centre 1/16 area | bottom half |
|---|---:|---:|---:|---:|
| real 800p 4:2:2 q3 | 2.34 | 1.22 | 0.88 | 1.78 |
| 720p 4:2:2 q3 | 5.02 | 3.11 | 2.32 | 4.23 |
| 1080p 4:2:2 q3 | 11.20 | 6.71 | 5.06 | 9.39 |

Rust decoders, single thread (`RAYON_NUM_THREADS=1`, `-C target-cpu=cortex-a76`), ms:

| Stream | zune RGB | zune Luma | jpeg-decoder RGB (Styx default) | turbojpeg GRAY (for reference) |
|---|---:|---:|---:|---:|
| real 800p 4:2:2 q3 | 6.38 | 3.74 | 20.42 | 2.36 |
| real 800p 4:2:0 q6 | 5.11 | 2.93 | 16.19 | 1.80 |
| 720p 4:2:2 q3 | 9.67 | 6.56 | 24.55 | 5.02 |
| 720p 4:2:2 q6 | 5.77 | 3.45 | 18.96 | 2.48 |
| 1080p 4:2:2 q3 | 22.02 | 15.00 | 55.66 | 11.19 |
| 1080p 4:2:0 q6 | 11.27 | 7.20 | 35.68 | 5.10 |

`TJPARAM_FASTDCT` saves a further 7–15% on the A76 (more than on x86), at a small accuracy cost.
It's worth exposing as an option.

### CM5-2. CPU access to PiSP output buffers

Capture was NV12 1280×800 + NV12 640×400 at 30 fps, 145 frames per row. All numbers are µs per
frame for the 1.02 MB Y plane.

| Buffers | Exporter | SYNC_START | SYNC_END | u8 loop | NEON 64 B | memcpy | Stale reads without sync |
|---|---|---:|---:|---:|---:|---:|---|
| `FrameBufferAllocator` (what Styx uses) | `videobuf2_dma_contig` | 0.8 | 0.6 | **10,170** | 590 | 665 | none (uncached) |
| dma-heap `linux,cma`, imported | `linux,cma` | 31.7 | 22.8 | **691** | **42** | 67 | yes, checksums vary between reads |
| dma-heap `system`, imported | `system` | 51.5 | 34.9 | 691 | 45 | 76 | 38/145 frames stale before `SYNC_START` |

For reference, the same u8 loop over a heap copy takes 690 µs, so cached dma-heap buffers match
normal memory. The sync cost (~55–87 µs/MB) is far below the 10 ms penalty of uncached scalar
reads.

### CM5-3. PiSP stream configuration and timing

- Validation results: see the table in [§2.1](#21-raspberry-pi-5--cm5-bcm2712-pisp).
- Frame matching: 150/150 Requests had both buffers, with equal `timestamp` and `sequence`.
- `SensorTimestamp` → request completion: 8.9 ms mean (CLOCK_MONOTONIC and CLOCK_BOOTTIME agree).
- `ScalerCrop` (0,0)/1280×800 → (320,200)/640×400 was reflected in metadata 4 frames after it was set.
- Controls exposed: `ScalerCrop`, `ScalerCrops`, `FrameDurationLimits` [33333..120000] µs.

### Still not measured

- RK3588, Jetson and N100 remain [E]/[D]. The only VA-API measurement is on the x86 host's AMD dGPU (M3).
- Pi 5 with a real USB MJPEG camera: vendor quantisation tables and bitrates may differ from the
  FFmpeg-encoded frames used here.
- Whether the second PiSP output adds latency versus a single stream. It isn't on the critical path.

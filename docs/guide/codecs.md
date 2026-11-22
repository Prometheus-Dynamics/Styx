# Codecs: encoders, decoders and converters

Every codec is one of three traits in `styx-codec-api`: `Encoder`, `Decoder` or `Converter`. Each
call takes the input lease and an output lease the caller acquired from its own pool. The codec
writes into the output, allocates nothing per frame, and copies only where its backend must.

## Choosing one: the registry

`Registry::select` tries the native candidates first (in registration order), then FFmpeg, and
returns the first that builds, with a decision for every candidate: chosen, skipped (and why), or
not tried. A request nothing satisfies fails with `SelectError`, which names both formats and
carries the same decisions. The registry logs nothing; the caller logs what it wants.

| Codec | Crate | In → out | Backend | State |
|---|---|---|---|---|
| JPEG encoder | `styx-jpeg` | I420, NV12, GREY, R8 → MJPG | native, pure Rust | host |
| H.264 encoder | `styx-h264` | raw → H.264 (Constrained Baseline) | native, pure Rust | host |
| MJPEG decoder | `styx-decode` | MJPG/JPEG → RGB24 | native: libturbojpeg (feature `turbojpeg`), else zune-jpeg | host |
| Raw converters | `styx-decode` | YUYV, NV12, I420 → RGB24; YUYV, NV12 → GREY | native | host |
| Scaler, software | `styx-scale` (`Software`) | any resize; exact conversions within a format family | native | host |
| Scaler, hardware | `styx-scale` (`Lemnos`) | resize, same format in and out, on a Lemnos `image.scaler/1` device (RGA, G2D) | native | untested on board |
| HEVC / H.264 decode | `styx-block` | Annex B → NV12, on a Lemnos `codec.block/1` stateless decoder | native | untested on board |
| H.264 encode, rkvenc | `styx-block` (`rkvenc`) | NV12 → H.264 IDR, on the `rk-mpp:rkvenc` block (VEPU540 class) | native | not usable until Lemnos supplies the VEPU540 encodings; untested on board |
| FFmpeg | `styx-ffmpeg` (feature `ffmpeg`) | MJPG ↔ I420; DRM PRIME frames from a hardware decoder | FFmpeg | host |

## Styx's own encoders

- **`styx-jpeg`:** baseline JFIF with the Annex K tables. Restart markers after every MCU row
  (`with_restart_rows`) make each row a slice that decodes on its own. `encode_blocks` codes DCT
  blocks computed elsewhere to the same bytes.
- **`styx-h264`:** Constrained Baseline with CAVLC, one slice and one reference frame. The motion
  search is a whole-pel diamond refined to quarter pels. Each picture has a fixed QP, or a QP
  chosen to hold a `RateTarget`.
  - Not yet: deblocking, a GPU motion search, and anything past Constrained Baseline.

Their arithmetic is `styx-simd` kernels, each tested equal to a scalar oracle.

## Scaling: `styx-scale`

Register the hardware first (`register_lemnos`), then the software (`register_software`).

- When the hardware cannot be built (no service, no scaler, the format not taken), its decision
  says why and the software is chosen. `reason` states it for a selection.
- The hardware path copies the input into the stream's input buffer and the completion into the
  output. `Stats` counts both copies.
- YUV to RGB is not here yet; it needs a colour matrix.

## Hardware decode: `styx-block`

Lemnos owns the block, and Styx owns the bitstream: the HEVC and H.264 parsers and the controls
they build.

- **HEVC:** stateless, one IDR slice per frame. Inter slices are refused until their reference
  picture sets are parsed.
- **H.264:** decodes by slice, with Lemnos's V4L2 H.264 control structures.
- **Recovery:** follows Lemnos's stateless decoder backoff.

## Hardware encode: `styx-block::rkvenc`

H.264 on a Rockchip VEPU541-class block (`rk-mpp:rkvenc`, the VEPU540 among them), one job
per frame through `codec.block/1`. Lemnos owns the register layout and the job; Styx owns the
bitstream: the SPS and PPS (`styx_h264::ParameterSets`, the same bytes as Styx's own encoder),
the slice's syntax values and the QP.

- **No copy:** the input and output leases are job stream buffers, named in the job by buffer and
  offset. The block reads the picture and writes the slice in place; Styx writes only the
  parameter sets at the start of the output. A frame allocates nothing.
- **Each frame is an IDR** (all intra, CAVLC, deblocking on), with `idr_pic_id` alternating.
- **Not usable until Lemnos supplies the VEPU540 encodings.** The layout gives each field's place,
  not what its values mean. `EncoderFacts` holds what the encoder needs (`rkvenc::MISSING` lists
  the six facts asked of Lemnos), and `vepu540_facts()` has none, so the registry candidate does
  not build and its decision says why.
- **What the gate proves:** the job's shape (every register Styx sets, every buffer reference on
  the leases, the read-back window), zero copy, no per-frame allocation, and the parameter set
  bytes, on rk-mpp's played kernel with the facts filled by test values. Not the encoded picture:
  the played kernel runs no encoder, and a stand-in puts the slice's length in the read-back.
- Untested on board.

## FFmpeg, for coverage: `styx-ffmpeg`

Nothing links FFmpeg: the libraries are opened at run time, the first time a candidate is built. A
machine without them reports `FFmpeg not present: ...`, and the registry skips those candidates
with that reason.

- **Copies:** frames are copied at the boundary, and each copy is counted.
- **PRIME frames:** a hardware decoder's DRM PRIME frames are the exception. `decode_prime` hands
  them on as a lease over the dma-buf itself, with no copy. Only frames with all planes in one
  dma-buf are taken.
- **Version:** the ABI is written against FFmpeg 8 (libavutil 60, libavcodec 62), Linux only.

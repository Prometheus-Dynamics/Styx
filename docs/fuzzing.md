# Fuzzing

Styx reads bytes it did not write: USB descriptors and payloads from whatever is plugged in,
embedded data lines from sensors, statistics buffers, recordings, tuning and sensor files, and
messages from other processes (the camera service, the frame socket). Every parser of such bytes
has a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) target in [`fuzz/`](../fuzz/), run
with AddressSanitizer and overflow checks. The rule the targets enforce: **untrusted input gives
an error, never a panic, a read outside a buffer, a hang, or an allocation sized from a header.**

## Targets

| Target | Input | What it runs |
|---|---|---|
| `uvc_descriptors` | bytes | USB device/configuration and UVC class descriptors (`styx_uvc::UvcFunction::parse`), then formats, frame sizes, interval lookups, alternate settings by bandwidth, announced controls, `find_mode` |
| `uvc_stream` | structured | payload headers and frame assembly from a stream of (lost, failed, reset) payloads, SCR/PTS clock recovery, PROBE/COMMIT decode/encode, control values |
| `sensor_description` | bytes: TOML, NUL, controls and an embedded line | `SensorDescription::from_toml_str`, timing and gain models, the driver over a mock bus (power, init, modes, flips, test patterns, scheduled controls), embedded-data decoding |
| `sensor_embedded` | bytes | embedded data lines through every shipped layout: OV9782 register lists, CCS/SMIA tagged lines (plain, RAW10 and RAW12 padded) |
| `sensor_subdev` | structured | descriptions built from a kernel driver's report (sizes, codes, control ranges, menus) and a kernel data TOML (`from_subdev_with`) |
| `algo_tuning` | bytes | Raspberry Pi tuning JSON (the lenient reader and the conversion) and Styx TOML tuning, then the algorithm pipeline built, prepared and run over simulated frames |
| `pisp_stats` | bytes | PiSP front end statistics buffers: decode, queries, both conversions for the algorithms, the default pipeline |
| `kernel_messages` | bytes: selector byte, then input | uevents, media topology arrays (every link resolved), V4L2 events and formats |
| `codec_raw` | structured | every raw decoder and converter (Bayer through the software ISP, YUV, NV12, mono, RGB) with odd sizes, strides and short planes |
| `mjpeg_decode` | bytes | MJPEG frames through the turbojpeg, turbojpeg-luma and zune decoders |
| `core_frame_layout` | structured | frame descriptors (format, sizes, offsets, strides) imported over memfds, dma-buf-like descriptors or host buffers, then every read, copy, crop, pyramid and re-allocation |
| `frame_socket_message` | bytes: descriptor shape byte, JSON | frame socket messages imported as a consumer imports them, every plane read |
| `ipc_messages` | bytes | camera service and frame client messages; decoded requests must encode back to themselves |
| `ipc_request` | bytes | camera service requests (wire format 4, `FrameRequest`): decoded, checked as the service checks them, planned on virtual cameras alone and shared |
| `replay_reader` | bytes | MCAP and `.styxrec` recordings (CDR messages included), every frame's pixels read |
| `pipeline_rawrec` | bytes: index, NUL, frames | `styx-pipeline` raw recordings and the virtual sensor replaying them |

Hooks the targets need inside crates are `#[doc(hidden)] pub fn fuzz_*` functions
(`styx::ipc::fuzz_messages`, `fuzz_service_request`, `frame_socket::fuzz_import`,
`styx_kernel::media::fuzz_topology`, `event::fuzz_event`, `v4l2::Format::fuzz_from_bytes`).

## Running

Needs a nightly toolchain and `cargo install cargo-fuzz`.

```bash
scripts/fuzz.sh                    # every target for 60 s (what CI runs)
scripts/fuzz.sh -t 1800 -j 8       # 30 min each, 8 at a time
scripts/fuzz.sh uvc_descriptors    # one target
cd fuzz && cargo +nightly fuzz run -O --debug-assertions uvc_descriptors corpus/uvc_descriptors seeds/uvc_descriptors
```

The script builds with `--debug-assertions` (integer overflow is a bug here), limits RSS to
2 GiB and single allocations to 512 MiB, fails an input that runs over 10 s, and passes each
target its dictionary and largest input size. Corpora grow in `fuzz/corpus/<target>` (ignored by
git); crashing inputs and logs land in `fuzz/artifacts/`. Reproduce and minimise one with

```bash
cd fuzz
cargo +nightly fuzz run -O --debug-assertions <target> artifacts/<target>/crash-...
cargo +nightly fuzz tmin -O --debug-assertions <target> artifacts/<target>/crash-...
cargo +nightly fuzz fmt -O <target> artifacts/<target>/crash-...   # structured inputs, as Debug
```

CI (`fuzz-smoke` in `.github/workflows/ci.yml`) runs `scripts/fuzz.sh -t 60` nightly and on
demand, and uploads `fuzz/artifacts/` when a target fails.

## Seeds and dictionaries

`fuzz/seeds/<target>` holds small committed seeds (a few KB each): the C270's descriptors,
recordings, wire messages, frame socket messages, embedded lines, kernel messages, a raw
recording. The script also copies the repository's own data in at start: the sensor
descriptions, the OV9782 and generic tunings, `rpi-minimal.json`, the C270 MJPEG frames, and
generated PiSP statistics buffers. Seeds written by code come from ignored tests; regenerate them
with

```bash
STYX_FUZZ_SEEDS=$PWD/fuzz/seeds cargo test -p styx --features replay-styxrec,frame-socket --lib write_fuzz_seeds -- --ignored
STYX_FUZZ_SEEDS=$PWD/fuzz/seeds cargo test -p styx-pipeline --lib write_fuzz_seeds -- --ignored
```

`fuzz/dicts/` has the USB/UVC descriptor tags (`uvc.dict`), the keys of the tuning files
(`algo_tuning.dict`) and of the sensor files (`sensor.dict`).

## Adding a target

1. Write `fuzz/fuzz_targets/<name>.rs` with `fuzz_target!`: raw `&[u8]` when the input is bytes
   (a file, a message, a buffer), a `#[derive(Arbitrary)]` struct when it is a value with a few
   numbers in it (a layout, a report). Exercise what consumers do with the parsed value, not
   only the parse: the bugs are usually there.
2. Add a `[[bin]]` to `fuzz/Cargo.toml` (the script lists targets from it), seeds in
   `fuzz/seeds/<name>`, and options or repository seeds in `scripts/fuzz.sh` if it needs them.
3. Crate internals: a `#[doc(hidden)] pub fn fuzz_*` next to the parser.
4. Every crash found: fix it where it is, and add the input as a regression unit test in the
   crate (not only in the corpus).

## Findings

The first runs (2026-10, after which every target ran clean for its time) found:

- **Out-of-bounds reads** decoding odd-width UYVY/YVYU/VYUY (the last 4:2:2 group of a line
  ran past it, past the frame on the last line) and single-plane NV12 (the chroma rows of an
  odd width are a byte longer than the stride the length check counted)
  (`crates/codec/src/decoder/raw/{yuv,nv12}.rs`).
- **SIGBUS** reading a memfd another process sent that is shorter than its descriptor: the
  mapping ran past the file's end (`crates/core/src/buffer/frame/shared_fd.rs`). Imports
  (`from_memfd_import`, `from_dmabuf_import`) now refuse planes outside the descriptors they came
  with and layouts that do not hold their format.
- **Allocations sized from headers**: copying a frame's visible rows allocated what its format
  claimed before checking its planes (a damaged `.styxrec` asked for 3.8 EB);
  `materialize_owned` sized copies from layout offsets and panicked on planes outside the
  backing; UVC frame buffers took `dwMaxVideoFrameSize` as given (now at most 4 bytes per pixel
  and 256 MiB).
- **Panics on overflow or bad ranges**: UVC continuous intervals with min above max (`clamp`
  panics) or huge steps, PiSP statistics zone counts, luma rows and crops with absurd strides,
  raw recording indexes (zero height, huge strides, offsets past the end, no frames).

The runs, on a 24-core x86-64 host shared with other builds (ASan, overflow checks):

| Target | Time | Inputs run |
|---|---|---|
| `uvc_descriptors` | 30 min | 22.2 M |
| `replay_reader` | 30 min | 4.5 M |
| `ipc_messages` | 30 min twice (request wire format 3, then 4) | 95.5 M, 323 M |
| `ipc_request` | 30 min | 1.4 M |
| `frame_socket_message` | 30 min | 15.3 M |
| `core_frame_layout` | 30 min | 34.1 M |
| `codec_raw` | 30 min | 1.8 M |
| `uvc_stream` | 15 min | 4.9 M |
| `sensor_description` | 15 min | 0.75 M |
| `sensor_embedded` | 15 min | 5.1 M |
| `sensor_subdev` | 15 min | 3.3 M |
| `algo_tuning` | 15 min | 0.28 M |
| `pisp_stats` | 15 min | 2.4 M |
| `kernel_messages` | 15 min | 6.2 M |
| `mjpeg_decode` | 15 min twice | 52 k |
| `pipeline_rawrec` | 15 min | 34.4 M |

One `mjpeg_decode` input timed out once under that load (22 s); alone it decodes (to an error)
in microseconds in every decoder, and the second run was clean.

Miri (`cargo +nightly miri test -p styx-core-rs --no-default-features --features std --lib`: the queue, SIMD
(scalar), buffer pool, frame layout and view, format and transform tests, 59 in all; the memfd
and dma-buf tests need syscalls Miri does not have) found nothing.

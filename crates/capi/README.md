# styx-capi

Styx's versioned C surface, `include/styx.h` (1.0), and a Rust wrapper that calls through it.

- **Core:** Styx's planner and capture. **Surface:** this crate's `extern "C"` exports, built as
  `libstyx_capi.a` and `libstyx_capi.so.1`. **Wrapper:** [`Camera`], [`Stream`], [`Frame`],
  linked statically.
- **Shared vocabulary:** status codes, handles, `LemnosBufferDesc`, the version handshake and
  message sinks are `lemnos-abi` 1.0's (`include/lemnos/abi.h`, a byte copy). A program using
  Lemnos's surface too sees each type once.
- **Granularity:** one call per frame (next, describe, release), never per pixel.
- **One form per process:** static fat LTO stays the default. The shared library is for
  consumers outside the image. With the `shared` feature the wrapper links the installed
  `libstyx_capi.so.1` (`STYX_CAPI_LIB_DIR`) and this crate exports nothing.
- **Versioning:** the major is in the SONAME, and `styx_check_version` checks major and layout
  hash. rustc's cdylib version script overrides named symbol versions, so none are used.

## Calls

| Call | What |
|---|---|
| `styx_version`, `styx_check_version`, `styx_features` | the handshake; no optional features in 1.0 |
| `styx_camera_count`, `styx_camera_name` | probe; cameras are named by index |
| `styx_camera_add_virtual` | a synthetic camera for tests without hardware |
| `styx_stream_open`, `styx_stream_close` | plan and start a stream (`fourcc`, largest size, fps; 0 for any) |
| `styx_stream_next` | the next frame's handle and `StyxFrameInfo`; `-ETIMEDOUT`, `-EPIPE` |
| `styx_frame_buffer` | its dma-bufs as a `LemnosBufferDesc`, fds borrowed until release |
| `styx_frame_plane` | a plane's bytes in this process, valid until release |
| `styx_frame_release` | give the frame back to its capture |
| `styx_control_get`, `styx_control_set` | controls by id: bool, int, uint, float |

A frame handle holds its frame (its buffer stays out of the capture's queue) until it is
released. Handles come from `lemnos-abi` slabs that reuse their slots: a stale handle is
refused (`LEMNOS_E_STALE_HANDLE`), and taking and releasing frames allocates nothing once the
slabs have grown to the frames held at once.

## Tests

`cargo test -p styx-capi` runs the surface through the wrapper on a synthetic camera. It also
compiles `tests/c/client.c` and links it against `libstyx_capi.a` and `libstyx_capi.so.1`, so
`cargo build -p styx-capi` must have built those first (`scripts/gate.sh` does; with
`STYX_CAPI_C_REQUIRED=1` a missing library fails instead of skipping). The header's layout hash
is checked against the Rust structs at compile time.

Not yet: fences (frames are complete when handed out), frames on Lemnos buffers, and the
per-frame cost measured on the CM5.

# Styx `next`: the user guide

How to get frames out of a camera with Styx `next`: which backend owns a camera, what processes
its frames, and every way the frames leave. [`../design.md`](../design.md) says why the
pieces are shaped this way; this guide says how to use them.

Every backend hands out the same frame type (`styx_frame::FrameLease`), the same controls and
the same metadata (`styx_controls::FrameMetadata`). An application written against one backend
reads any other the same way.

## Pages

- [cameras.md](cameras.md): the camera backends (native on Lemnos, native USB, V4L2,
  libcamera, the simulated camera), how one owner is chosen per camera, and hotplug.
- [isp.md](isp.md): turning raw frames into pictures (soft ISP, GPU ISP, PiSP, Rockchip, K230,
  ISP600), the neutral settings every ISP reads, outputs, stills, and 3A.
- [ways-out.md](ways-out.md): the camera service and its frame sockets, then every reader of
  them (in-process Rust, other processes, C, GStreamer, PipeWire, Orion, Daedalus) and
  multi-camera grouping.
- [codecs.md](codecs.md): encoders, decoders and converters (Styx's own JPEG and H.264, MJPEG
  decode, scaling, hardware decode and encode blocks, FFmpeg for coverage).
- [tools.md](tools.md): `styx-measure`, recording and replay, previews, calibration
  (`styx-tune`), counters, DNG, and the examples.
- [dev-to-next.md](dev-to-next.md): what each `dev` crate and tool becomes on `next`, and what
  is not carried over. Replacing `dev` with `next` is the user's call; this page is the map.

## Where things run today

"Run on a board" means a run on real hardware with its results recorded. Everything else is
written and tested on the host (simulated hardware, or the host's own devices) and is marked
**untested on board**.

| Part | State |
|---|---|
| Native camera (CSI sensor through Lemnos) | run on a board: Raspberry Pi CM5 carrier, OV9782, 640x400 binned at 239.9 fps with 0 gaps, service CPU and RSS measured, reader hold limits checked (H1 pass 3) |
| Camera service, frame socket, `styx-measure` | run on a board (the same pass) |
| PiSP back end on the live stream (`isp = "pisp"`) | simulated ISP only; board run pending |
| Soft ISP | host; on the board as `styx-measure`'s in-reader ISP |
| GPU ISP | host (lavapipe, and a desktop GPU by hand) |
| Rockchip, K230, ISP600 ISPs | untested on board |
| Native USB, V4L2, libcamera | host (V4L2 on `vivid`); untested on board |
| Hardware decode blocks, hardware scaler | untested on board |
| Hardware H.264 encode (rkvenc) | not usable until Lemnos supplies the VEPU540 encodings; untested on board |
| GStreamer, PipeWire, C API, Orion, Daedalus | host |

## Quick start: one camera to another process

1. Write a service config (every key: [`styx-service`'s `config` module](../../crates/out/service/src/config.rs)):

   ```toml
   socket_dir = "/run/styx"
   lemnos_socket = "/run/lemnos/lemnosd.sock"

   [camera."lemnos:sensor0"]
   width = 640
   height = 400
   fps_milli = 60000
   ```

2. Run `styx-service <config.toml>`. A systemd unit is in
   `crates/out/service/systemd/styx-service.service`.
3. Ask the control socket for the camera list: `list` answers one `camera <id> ...` line per
   camera, with its frame socket (`socket=`).
4. Read that socket: `styx-measure read` (a measurement), `styxsrc socket=<path>` in GStreamer,
   the PipeWire node, `styx_socket::Reader` in Rust, or `include/styx.h` in C.

Without hardware, `--sim` on the examples starts a simulated camera chain in the process
([tools.md](tools.md#examples)).

# Tools: measuring, recording, previews, calibration, counters

## `styx-measure`: what a reader gets

The board-side reader of a camera's frame socket. Every figure comes from the board's own clocks
and counters, and a figure the board does not report is `null`.

```
styx-measure read --socket <camera frame socket> (--frames N | --seconds S)
                  [--control <service control socket>] [--service-pid PID]
                  [--soft-isp <FOURCC>[:WxH]] [--report <file.json>] [--trace <file.csv>]
                  [--start newest|next-new] [--hold N | --keep N | --stall-after N]
styx-measure status --control <service control socket>
```

`read` attaches as a reader in its own process and measures:

- **Frames and loss:** frames delivered, gaps, duplicates, order, and the frames the ring skipped
  for it.
- **Rate:** requested, achieved by the backend and in the metadata, and measured by the service
  and by the reader, with the reason when they differ.
- **Latency:** capture to reader (min, median, p99, max), and the same per maker of the frame
  (camera, PiSP, soft ISP).
- **Keeping up:** the longest time from one frame's capture to the reader having the next.
- **Copies:** copies per site, with the bytes read and written.
- **Cost:** the reader's CPU per frame, and both processes' memory.

`--soft-isp` runs the software ISP in the reader. `--hold`, `--keep` and `--stall-after` make it
hold frames, to check the producer's reserve rule from outside. A processed socket (`isp0`,
`isp1`) finds its camera's status line by itself. The read loop allocates nothing per frame.

`status` prints the service's `list` answer as it is.

## Recording and replay: `styx-record`

- **`Recorder`:** writes `styxrec`. That is a checksummed header (device, backend, format, buffer
  size), then one record per frame: identity, timestamp, format, planes and metadata, checksummed,
  followed by the frame's bytes as the producer wrote them.
- **Data channels** (format version 2): streams of timestamped samples recorded beside the
  frames, such as an IMU, a pose or a message from another process. Styx reads no sensors; the
  consumer that has the data writes it.
  - `Recorder::declare(ChannelSpec)` declares a channel once and returns its `ChannelId`. The
    spec has a name, the samples' encoding, and a schema (name, encoding, bytes), as MCAP
    names them, plus the producer's clock. Both clocks are read when it is declared and kept.
  - `Recorder::sample(id, timestamp_ns, bytes)` writes one sample (at most 1 MiB, CRC over its
    head and bytes). `Recorder::sample_batch(id, iter)` writes several of one channel in one
    call. Neither allocates.
  - Every timestamp in a recording is `CLOCK_MONOTONIC` ns. A producer on `CLOCK_BOOTTIME`
    converts with `ClockPair::now()` and `monotonic_from_boottime` (or the free function of
    that name, which reads the pair each call).
  - Write records as they arrive. Replay sorts them.
- **`Replay`:** hands a recording out as `FrameLease`s from its own pool, with the recorded
  identities, timestamps and metadata. It has a camera's shape: a pool, and `next_frame`.
  - `next_item` hands out frames and samples (`Item::Frame`, `Item::Sample`) in timestamp
    order, with one frame of lookahead. A sample at or before a frame's time comes before it.
  - A sample older than a frame already handed out is late: it comes out in file order,
    marked `late`, counted in `late_samples()`, never dropped.
  - The samples of a channel between frame n-1 and frame n are the ones that come out between
    them; the last of them is the nearest at or before frame n. The loop is in `next_item`'s
    documentation.
  - `channels()` / `channel_named` give the declarations. Those at the start of the file are
    read when it opens.
  - Version 1 files (frames only) still read. A build that reads only version 1 refuses a
    version 2 file as `UnsupportedVersion(2)`.
  - Neither path allocates once warm (tests/steady_state.rs counts).
- **`export_mcap(&mut replay, out, ExportOptions)`:** writes a `styxrec` recording as MCAP for
  MCAP tools, in the Styx MCAP layout (`ros2` profile, CDR). Chunks are lz4 by default
  (`McapCompression::None` for none); each chunk has a message index, and the file has a summary
  (schemas, channels, statistics, chunk indexes) so readers can seek.
  - Frames: `/styx/frame_meta` (`styx/msg/FrameMeta`) and `/styx/image` (`sensor_msgs/msg/Image`,
    rows packed) or `/styx/image/compressed`, at the frame's timestamp.
  - Data channels: one MCAP channel each, with their encoding, schema and metadata, plus
    `styx.clock = CLOCK_MONOTONIC` (every log time) and `styx.source_clock` (the producer's
    clock). Each sample is a message at its time.
  - `/styx/clock`: one record per declaration, with the clocks it recorded.
  - Not carried: the frame's control values (the exposure and gains it was taken with are), row
    and plane padding, a frame id above `u32::MAX`, a sensor timestamp other than the frame's, a
    lux or colour temperature of exactly 0. `ExportStats::losses` counts the frames each of
    these applied to.
- **`import_mcap`:** converts MCAP recordings in the Styx layout to `styxrec`.
  - Every topic outside `/styx/` becomes a data channel: name, message encoding, schema and
    metadata; each message a sample at its log time.
  - A channel with metadata `clock = CLOCK_BOOTTIME` has its times moved to `CLOCK_MONOTONIC` by
    the `/styx/clock` records (the latest before each message; the first in the file before
    it). With none, the clocks are taken as equal and counted
    (`losses.boottime_assumed_equal`). A channel with `styx.clock` (an export) is already
    monotonic.
  - `/styx/clock` (`styx/msg/ClockSample`, CDR): encapsulation `00 01 00 00`, then `u64
    monotonic_ns`, `u64 boottime_ns`, `u64 realtime_ns`, `u32 read_spread_ns`.
  - `ImportStats::losses` counts what is not carried: attachments, metadata records other than
    `styx.recording`, data messages' sequences (those other than the channel's count 0, 1, 2,
    ..., which is what export writes) and differing publish times, pyramid messages, other
    `/styx/` topics, data messages over 1 MiB.
  - Import then export then import gives the same frames (rows packed) and the same channels
    and samples (tests/mcap_export.rs, read back by the `mcap` crate too).
  - It reads uncompressed chunks, and lz4 chunks with the `mcap-lz4` feature (on by default, a
    pure-Rust codec, which export also uses).
  - A zstd chunk is refused by name, and so is an lz4 chunk in a build without the feature.
  - A decompressed chunk must have the size it states (at most 256 MiB), and its CRC when it
    gives one.

### The `styx-record` command

The crate builds a command for a person holding a recording (no Rust to write). It runs on a PC
and on a board: `cargo build --release -p styx-record --bin styx-record`, and for an aarch64 board
`--target aarch64-unknown-linux-musl` with `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`
(a static binary of about 610 KB; the crate has no C dependencies).

```bash
styx-record info rec.styxrec
styx-record export rec.styxrec rec.mcap [--uncompressed] [--chunk-size BYTES]
styx-record import rec.mcap rec.styxrec
```

- **`info`:** the header (device, keys, backend, format, interval, buffer size), the frame count
  and time span, each channel with its id, encoding, schema, producer's clock, sample count, the
  clocks recorded at its declaration and its metadata, the late samples, and how the file ended:
  - `clean`: finished with the end marker (`Recorder::finish`).
  - `not finished`: no end marker, but no record was cut (the recorder stopped between
    records).
  - `truncated`: cut inside a record; replay ends at the last whole one.
- **`export`:** writes MCAP (lz4 chunks unless `--uncompressed`), then the counts and the
  `ExportStats::losses` table.
- **`import`:** writes `styxrec`, then the counts and the `ImportStats::losses` table.

Exit status 0 on success (also when the output pipe closes, as with `| head`), 1 on a file it
refuses or cannot read or write, 2 on bad arguments. The output is the same on x86_64 and
aarch64 (tests/cli.rs runs the commands; the aarch64 build was checked under qemu).

Headers and records are untrusted input: every length is checked against a limit before it is
used, and a recording cut short ends at its last whole frame and says so.

What a channel costs the recorder (`examples/channel_cost.rs`, x86_64 host):

| | |
|---|---|
| `Recorder::sample`, 24 / 48 / 256 B | 42 / 55 / 178 ns |
| 120 fps 1280x800 GREY to `/dev/null`, recorder thread, 15 s x3 | 2.12% of a core; with a 1 kHz channel of 24 B samples 2.72% sent one by one, 2.21% in batches of 8 (`sample_batch`) |
| the same to an SSD, three 20 s runs each | 7.0–7.2% without the channel, 5.5–7.8% with; no drops from the channel |

The cost of a 1 kHz channel is the recorder thread waking once per sample; the write itself is
the 42 ns. Handing samples over in batches and writing them with `sample_batch` pays that wake
once per batch. Frames
were dropped only in some disk runs, with or without the channel, when the disk stalled.

## Previews: `styx-preview`

`Preview` is a low-priority consumer for user interfaces:

- It takes a share of a frame, keeps only the newest, and encodes a small JPEG with `styx-jpeg`
  on its own thread.
- It publishes the JPEGs on its own `styx-socket` ring.
- Frames over `max_fps` are dropped on arrival.
- It never waits and never restarts anything.

Reading the camera's ring, use `offer_held` with the `Frame` itself, so its hold lasts as long as
the preview's share. Serving MJPEG or WebSocket to a browser is the application's job: a reader
of the preview ring gets the JPEG bytes in place.

## Controls from another process: `styx-control-client`

`ControlClient` speaks the camera service's control socket (the protocol is in
`styx_service::control`):

- **The calls:** the camera list, one camera's status line, its controls (as the service's lines
  or as the in-process `ControlsInfo`), standard and device sets, the 3A loop's controls by name,
  the last frame, start, and stop.
- **One connection per request.** The service serves one connection at a time and hangs up on
  one idle for 2 s.
- **Time limits:** each request has a timeout (5 s by default). An unreachable socket is retried
  on a backoff from 250 ms, doubling, capped at 5 s; calls before the next try fail at once.
- **Waiting for a change:** `wait_controls_change` polls the camera's `controls_seq` every 100 ms
  by default (never under 20 ms) until it moves.
- **Checked requests:** ids, names and values that could add a request of their own (white space,
  control characters, too long) are refused before anything is sent.
- **Errors are typed** by `ErrorKind`, with the service's text.

`styx-capi`'s service source uses it.

## Calibration: `styx-tune`

Raw captures of standard targets go in, and a tuning file the tuning search finds comes out
(`styx-tune calibrate`, or the library). A session file says what each capture shows: dark, flat
field, ColorChecker, grey card, static scene. From those it calibrates:

- black level per channel and gain, and hot pixels;
- lens shading per colour temperature;
- the AWB curve and colour matrices (chart found automatically, or by corners);
- the noise profile, the lux reference, and green equalisation.

Output is styx-aaa TOML in a tuning directory, plus a report. `synth` makes a synthetic sensor
to check that calibration gives back what the frames were made with.

## Counters: `styx-counters`

Per-camera counters any process can read:

- frames delivered;
- drops by reason;
- copies;
- frame age from capture to publish (min, median, max, and a 32-bucket histogram);
- the achieved rate against the requested one;
- the owner and why, health, state and backoff.

The frame path uses atomics only. A `Hub` publishes one fixed-layout snapshot per camera on a
ring, and a `Subscriber` in another process reads them. There is no HTTP server.

## DNG: `styx-dng`

- **`write_dng`:** raw Bayer or mono frames, at 8 to 16 bits or CSI-2 packed, to DNG 1.4. It
  writes black and white levels, the as-shot neutral, colour calibration at up to two
  illuminants, lens shading as `GainMap` opcodes, EXIF and a preview.
- **`read_dng`:** reads what calibration needs from this writer's DNGs or a camera's.

## Examples

In `examples/`, built by `styx-examples` (`cargo run -p styx-examples --example <name> -- --sim`):

| Example | What it shows |
|---|---|
| `quickstart` | open the first camera and read ten frames |
| `capture` | set exposure and gain for one named frame, and see them land there |
| `socket_reader` | read a camera's frames from the service's socket, in another process, with no copy |
| `daedalus_frames` | ring frames fed into a Daedalus graph as `styx:frame` values (feature `daedalus`) |

With `--sim` an example starts a simulated camera on `lemnosd` in its own process. Otherwise it
reads the Lemnos daemon named by `--lemnos SOCKET`, and libcamera and V4L2. Either way the camera
is opened the way the service opens it.

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
    head and bytes). It allocates nothing.
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
- **`import_mcap`:** converts MCAP recordings in the Styx layout to `styxrec`. Import only.
  - It reads uncompressed chunks, and lz4 chunks with the `mcap-lz4` feature (on by default, a
    pure-Rust decoder).
  - A zstd chunk is refused by name, and so is an lz4 chunk in a build without the feature.
  - A decompressed chunk must have the size it states (at most 256 MiB), and its CRC when it
    gives one.

Headers and records are untrusted input: every length is checked against a limit before it is
used, and a recording cut short ends at its last whole frame and says so.

What a channel costs the recorder (`examples/channel_cost.rs`, x86_64 host):

| | |
|---|---|
| `Recorder::sample`, 24 / 48 / 256 B | 42 / 55 / 178 ns |
| 120 fps 1280x800 GREY to `/dev/null`, recorder thread | 1.9% of a core; 2.4% with a 1 kHz channel of 24 B samples |
| the same to an SSD, three 20 s runs each | 7.0–7.2% without the channel, 5.5–7.8% with; no drops from the channel |

The cost of a 1 kHz channel is the recorder thread waking once per sample (about 5 us each in
the example, which sends each sample on its own); the write itself is the 42 ns. A consumer that
hands samples to its recorder thread in batches pays that wake once per batch. Frames
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

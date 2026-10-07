# Multi-camera sync

Styx can hand an application the frames of several cameras taken at the same moment. It does
this two ways:

- **In software**, for any cameras: `styx::multicam::FrameGrouper` groups frames by their sensor
  timestamps. This works on any hardware. Free-running cameras are only as close as their phase
  allows: up to half a frame period apart, and the gap wanders as their clocks drift.
- **In hardware**, for sensors with a frame-sync input: a common trigger starts every sensor's
  exposure together. The grouper then sees spreads of microseconds, and its metrics show it.

This page covers the grouper, its metrics, the Daedalus integration, and what hardware sync of
the OV9782 / OV9281 / OV9282 on a CM5 would need.

## Grouping frames: `styx::multicam`

```rust
use std::time::Duration;
use styx::multicam::{FrameGrouper, GroupConfig, GroupPolicy, RateMatch};
use styx::prelude::*;

let config = GroupConfig::new(16_000_000)  // tolerance: half a 30 fps frame
    .policy(GroupPolicy::Strict)            // every camera, or nothing
    .rate(RateMatch::Nearest)               // same-rate cameras
    .deadline_ns(100_000_000)               // wait at most 100 ms for a late camera
    .depth(3)                               // frames held per camera
    .output_depth(2);                       // groups held until taken
let mut grouper = FrameGrouper::new(config)?.named("stereo");
let left = grouper.add("left", left_capture)?;    // CaptureHandle
let right = grouper.add("right", right_client)?;  // FrameClient of a camera service
while let RecvOutcome::Data(group) = grouper.recv(Duration::from_secs(1)) {
    let (l, r) = (group.get(left), group.get(right));
    println!("spread {} µs, complete {}", group.spread_ns / 1000, group.complete);
}
```

**Sources** (`GroupSource`):

| Source | How it is woken |
|---|---|
| `CaptureHandle` | Its queue's waker, through `CaptureHandle::poll_recv`. |
| `MediaPipeline` | The same, through `MediaPipeline::poll_next`. Processing runs on the grouper's thread. |
| `FrameClient` | Its descriptor, polled through `try_client_event`. Its `Connected` and `Disconnected` events mark the camera present or absent. |
| `BoundedRx<FrameLease>` | Its waker. Use it for synthetic cameras, or frames forwarded from another thread. |
| Your own type | Implement `GroupSource`: `poll_event(&Waker)`, plus optionally `fd()`. |

**APIs.** These mirror `FrameClient`:

- `try_next()` never blocks.
- `recv(timeout)` blocks for up to `timeout`.
- `next().await` and `stream()` work on any executor (styx-graph's reactor).
- The grouper's own descriptor (`AsFd`) is readable when it has work to do: a frame arrived, a
  camera connected or disconnected, or a group deadline passed. It is an epoll set holding:
  - an eventfd, written by the in-process queues' wakers;
  - a timerfd, set to the next deadline;
  - the descriptors of any `FrameClient`s.
- `try_event()` and `recv_event()` also report `GroupEvent::Connected(i)` and
  `GroupEvent::Disconnected(i)`.

### The algorithm

The algorithm lives in `styx_core::multicam`. It is `no_std` + `alloc`, has no clock of its own,
and is generic over the item it groups.

1. **Anchor.**
   - `RateMatch::Nearest` uses the oldest pending frame of any connected camera.
   - `RateMatch::Slowest` uses the oldest frame of the slowest camera. The frame period is
     measured from each camera's timestamps, so a 60 fps camera paired with a 30 fps one gives
     every other frame. Its frames in between are dropped as `Unmatched`.
2. **Match.** Every other camera contributes its frame nearest the anchor, if that frame is within
   `tolerance_ns`. Styx waits until the choice is settled: the camera has a frame at or after the
   anchor, or its next frame (predicted from its period) would be farther away. Waiting is limited
   by the deadline. Older frames of that camera are dropped as `Superseded`. If the anchor's own
   next frame is nearer to a match, the anchor gives way (also `Superseded`).
3. **Policy.**

| `GroupPolicy` | Emits | Missing camera |
|---|---|---|
| `Strict` | Groups with every registered camera, connected or not, queued in order. | The anchor is dropped: `Incomplete` (too late, or disconnected) or `Unmatched` (nothing within tolerance). |
| `Partial` | Whatever matched by the deadline, if it has at least `min_members` frames (default 2). `FrameGroup::complete` marks full groups. | Not waited for once disconnected. |
| `Latest` | Only the newest complete group of the connected cameras. A newer group replaces one not taken yet (`Stale`). | Disconnected cameras are not needed. |

**Bounded memory.**

- Each camera holds at most `depth` pending frames. The oldest goes first (`Overflow`), which
  gives latest-frame semantics.
- At most `output_depth` groups wait to be taken (1 for `Latest`). Older groups go `Stale`.
- A dropped frame is released at once, so its capture buffer goes back to the pool before the
  grouper returns.
- `FrameGrouper::held()` is never more than `cameras × (depth + output_depth)`. The tests check
  that pool lease counts return to zero.

**Cameras coming and going.**

- A disconnected camera's pending frames are released (`Disconnected`), and its timestamp history
  is forgotten.
- A frame arriving later, or a `Connected` event, brings it back.
- `remove(i)` takes it out for good, so groups stop needing it.
- A source that closes (a capture stopped, a non-reconnecting client gone) counts as
  disconnected. Once every source has closed, `try_next` returns `Closed`.

**Late frames.** A frame is dropped as `Late` in either of these cases:

- its timestamp is more than `tolerance` before the last emitted group's anchor;
- its timestamp goes back in time on its own camera.

### Clocks

Timestamps are compared on one clock (`ClockMode`). What each backend gives (docs/timestamps.md;
the hop metrics' `sensor` hop is the same timestamp converted to `CLOCK_MONOTONIC`):

| Backend | `FrameMeta::timestamp` | `FrameMeta::clock` |
|---|---|---|
| native (`rp1-cfe`, `unicam`) | Receiver frame start: the first line received | `Monotonic` |
| libcamera | `SensorTimestamp` (start of exposure of the first line) when reported, else the buffer timestamp | `Boottime` (or the clock `StyxConfig::timestamp_clock` converts to) |
| V4L2 | The driver's buffer timestamp (`uvcvideo`: first payload, or PTS-based) | `Monotonic` when the driver flags `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC`, otherwise `None` |
| virtual, netcam | Arrival in Styx | `StreamRelative` unless `timestamp_clock` is set |
| file, simulation, replay | Media time | `StreamRelative` |

These are not the same instant in a frame's life. On native captures the timestamp is the end of
exposure plus readout of the first line (frame start). On libcamera it is the start of exposure.
Grouping a native camera with a libcamera one leaves an offset of about the exposure time. The
offset metrics show it, and the drift estimate is unaffected.

The modes:

- **`ClockMode::Common(clock)`** (default `Monotonic`). Every frame is converted to `clock`.
  - The offset between two system clocks is sampled once a second. Monotonic and boottime differ
    only by time spent in suspend.
  - Frames with no clock (`None`) or a `StreamRelative` clock are refused. They are dropped as
    `Clock`, and `last_error()` names the camera and the reason.
- **`ClockMode::Arrival`**. Uses the time Styx took the frame from its driver: the `dequeued` hop,
  which travels with frames across processes. It falls back to arrival at the grouper. Use it for
  sources without a sensor clock. The spread then includes delivery jitter.
- **`ClockMode::Raw`**. Compares timestamps as they are, for sources sharing a timeline Styx
  cannot name, such as one replay or a trigger counter. Every frame must report the clock of the
  first one (`ClockError::Mismatch`).

## Sync quality metrics

`FrameGrouper::report()` returns a `SyncReport`. `FrameGrouper::metrics()` returns the same with
the grouper's and the cameras' names. Every named grouper (`.named("stereo")`) is listed in
`styx::metrics::snapshot().sync_groups` while it lives. It is exported in Prometheus text
(`MetricsSnapshot::prometheus_text()`, `serve_http`) with label `group`, and `camera` per camera:

| Metric | What | How |
|---|---|---|
| `styx_sync_groups_total{kind}` | Groups formed: `complete`, `partial` (missing a camera), `stale` (replaced before taken) | Counted when emitted or replaced |
| `styx_sync_match_ratio` | Frames that went into groups ÷ frames received | `SyncReport::match_rate` (0.5 for a 60 fps camera's frames paired with a 30 fps camera) |
| `styx_sync_spread_ms{quantile}` | Latest minus earliest timestamp in a group: `0.5`, `0.99`, and `1` (window max) | The last 256 groups (`SPREAD_WINDOW`). `spread_last_ns` and `spread_max_ever_ns` are in the report. |
| `styx_sync_frames_total{camera,stage}` | `received`, `grouped` | Per camera |
| `styx_sync_drops_total{camera,reason}` | Frames dropped: `unmatched`, `superseded`, `incomplete`, `overflow`, `late`, `stale`, `clock`, `disconnected` | `DropReason`, per camera |
| `styx_sync_offset_ms{camera}` | Camera timestamp minus the reference camera's (`GroupConfig::reference`, default the first camera), in the last group with both | Per group |
| `styx_sync_offset_mean_ms{camera}` | The same, averaged over the drift window | Exponentially weighted, `drift_window` groups (default 512) |
| `styx_sync_drift_ppm{camera}` | How fast that offset changes: the camera clock's drift against the reference, in µs per second | Weighted least-squares slope of offset over reference time (`DriftEstimator`), re-centred at every sample |
| `styx_sync_frame_period_ms{camera}` | Frame period measured from timestamps | Smoothed. A gap of n periods counts as n periods. |
| `styx_sync_camera_connected{camera}` | 1 while connected | |

With `metrics-serde`, `SyncReport` and `SyncGroupMetrics` are serde types. `MetricsSnapshot` gains
`sync_groups`, with a serde default, so older readers and writers still interoperate. The
`styx::ipc` wire version is unchanged.

## Daedalus: one tick per group

Daedalus (`dev` at `ed7ddde`) synchronizes multi-input ticks through its host bridge.

- `HostBridgeHandle::push_batch` and `HostGraph::batch()` enqueue several payloads under one
  bridge lock and wake the graph once.
- A tick takes all host inputs under that lock, so it sees a batch whole or not at all.
- `HostGraph::inbound_fd()` and `tick_ready()` let a host wait on graph input in the same
  `poll(2)` as its own descriptors.

Styx feeds this as follows (`styx_core::daedalus`, feature `daedalus`):

- **`push_group(host.host(), &ports, group)`** pushes each camera's frame to its port as
  `frame_payload` (zero copy, same `styx:framelease` key, same adapters) in one batch.
- **`GroupPorts`** maps camera indices to host input ports. `.info("sync")` also pushes a
  `FrameGroupInfo`.
- **`FrameGroupInfo`** (`styx:frame_group`, registered by `StyxFramesPlugin`) carries:
  - `sequence`, `reference_ns`, `spread_ns`, `complete`;
  - `cameras` present;
  - `offsets_ns`.
- **`group_payloads`** builds the same batch for callers that add their own context values to it.

A node taking every camera's frame sees them from the same instant:

```rust
#[node(id = "app.stereo", inputs("left", "right", "sync"), outputs("depth"))]
fn stereo(left: &FrameLease, right: &FrameLease, sync: &FrameGroupInfo) -> Result<Depth, NodeError>
```

A `Partial` group leaves a missing camera's port empty for that tick. Nodes that must run anyway
should take `Option<&FrameLease>`. Otherwise use `Strict` or `Latest`.

A driver loop can wait on both the grouper's descriptor and `host.inbound_fd()`.
`examples/02_graph/daedalus_multicam.rs` is the whole flow with two virtual cameras:

```bash
cargo run -p styx-examples --features daedalus --bin daedalus_multicam
```

## Hardware sync: OV9782 / OV9281 / OV9282

Sources:

- [S] Styx's `crates/sensor/sensors/ov9782.toml`.
- [K] The Raspberry Pi kernel tree at `linux-rpi-7.2`:
  - `drivers/media/i2c/ov9282.c`;
  - `arch/arm/boot/dts/overlays/` (overlays and their `README`);
  - `arch/arm64/boot/dts/broadcom/`.
- **[datasheet?]** marks anything those sources do not state. It must be checked against the
  OmniVision datasheet, or measured, before it is relied on.

### What the sources show

**The sensor's sync pins.**

- The family has an FSIN (frame sync) input.
- The RPi `ov9281` overlay README says, for `trigger-mode`: "In this mode, the sensor outputs a
  frame only when triggered by a rising edge on the FSIN input pin" [K: overlays/README,
  `trigger-mode`].
- The driver defines output enables in `OUTPUT_ENABLE6` (0x3006) [K: ov9282.c, `OV9282_OUTPUT_ENABLE6_*`]:

| Bit | Name |
|---|---|
| 7 | `D0` |
| 6 | `PCLK` |
| 5 | `HREF` |
| 3 | `STROBE` |
| 2 | `ILPWM` |
| 1 | `VSYNC` |

- Both Styx's init and the driver's write 0x3006 = 0x04, which is `ILPWM` only, so STROBE and
  VSYNC are **off** [S: `init`, `[0x3006, 0x04]`; K: `common_regs`].

**External trigger (FSIN slave) mode.**

The driver enables it when the device tree property `trigger-mode` is > 0. The `ov9281` overlay's
`trigger-mode` parameter sets it; it is an RPi downstream property, not in the upstream binding.
After the common, mode and control registers, `ov9282_apply_trigger_config()` writes:

| Register | Value | Driver's name / comment |
|---|---|---|
| 0x0100 | 0x00 | `MODE_SELECT`: standby |
| 0x4F00 | 0x01 | `POWER_CTRL`: "Low power mode" |
| 0x3030 | 0x04 | `LOW_POWER_MODE_CTRL`: "External trigger snapshot" |
| 0x303F | 0x01 | `NUM_FRAME_ON_TRIG`: "1 frame per trigger" |
| 0x302C | 0x00 | `SLEEP_PERIOD_CTRL0` |
| 0x302F | 0x7F | `SLEEP_PERIOD_CTRL3` |
| 0x3823 | 0x00 | `TIMING_23`: "No auto wake" |

Then it leaves 0x0100 at standby: "stay in standby mode and wait for trigger signal"
[K: ov9282.c, `ov9282_apply_trigger_config`, `ov9282_enable_streams`].

In normal mode, the init writes 0x3030 = 0x10 [S, K]. The driver has **no** master mode that
drives FSIN or VSYNC out for other sensors, and no V4L2 control for sync. Trigger mode comes only
from the device tree.

**Strobe output.**

- `V4L2_CID_FLASH_STROBE_OE` sets bit 3 of 0x3006.
- `V4L2_CID_FLASH_DURATION`, in µs, writes the 32-bit `STROBE_FRAME_SPAN` at 0x3925–0x3928 as
  `µs × 192 / (width + hblank)`. Its default is 0x1a [K: ov9282.c, `OV9282_REG_STROBE_FRAME_SPAN`,
  `OV9282_STROBE_SPAN_FACTOR`].
- The driver says the step width is not documented. It writes no strobe offset or shift.

**What Styx's ov9782.toml has.** It contains no sync, strobe, FSIN or VSYNC registers [S]. It does
add 0x4F00 = 0x08 (PSV_CTRL bit 3: auto power-save off, measured necessary above 4096 lines).
The driver's trigger mode writes 0x4F00 = 0x01. **The two conflict.** How to combine them (for
example 0x09) is **[datasheet?]**.

**Other sensors on RPi, for comparison.**

- `imx477` has `cam0-sync-source` / `cam0-sync-sink` overlay parameters. As a source it drives
  XVS out (0x4b81, 0x3040). As a sink it takes it in (0x3041 `MS_SEL`, 0x3f0b `MC_MODE`)
  [K: imx477.c].
- `imx296` has `sync-sink` (XTRIG) [K: imx296.c, overlays/README].
- The RPi `cam0_sync` / `cam1_sync` dtparams drive a GPIO high at frame start. They exist only
  for Unicam (bcm270x); no bcm2712 (CM5) equivalent turned up [K: overlays/README,
  `bcm270x-rpi.dtsi`].

### What the sources do not settle: [datasheet?]

- **FSIN timing.**
  - Polarity and edge: the README says rising edge; whether the polarity can be configured is
    unknown.
  - Minimum pulse width.
  - Trigger-to-exposure latency and its jitter.
  - How long the sensor needs before it accepts the next trigger.
- **Continuous slave mode.** Whether FSIN can lock a free-running (non-snapshot) stream: a
  "frame sync" mode where FSIN resets the frame timing instead of starting one snapshot. The
  0x3823–0x3827 timing registers and 0x4242 are often named for this on OmniVision parts, but
  neither source writes them.
- **VSYNC out as master.** Whether 0x3006 bit 1 alone puts VSYNC out on a pin a second sensor's
  FSIN can take, and the pulse width and polarity. The output enable is defined but unused.
- **Strobe pin.** Which pin carries STROBE, and its polarity, shift and width semantics beyond
  the driver's span formula.
- **I/O voltage.** The sensor's I/O voltage on the modules used (DOVDD, often 1.8 V), and so FSIN's
  input threshold.
- **The module.** Whether a given OV9782 or OV9281 module breaks FSIN and STROBE out at all, on
  which connector or pads.

### What a CM5 board needs

- **A path for FSIN.** The 15- and 22-pin MIPI CSI camera connectors carry the clock and data
  lanes, I²C and a `CAM_GPIO` enable line. They have no FSIN or strobe pin.
  - On the CM5, `CAM_GPIO` is RP1 GPIO 34, and both camera ports share it (`cam1_reg: &cam0_reg`)
    [K: bcm2712-rpi-cm5.dtsi]. It is a power enable, not a trigger.
  - The I²C buses are `i2c6` (CAM/DISP 0, pins 38/39) and `i2c0` (CAM/DISP 1), at 100 kHz
    [K: bcm2712-rpi-cm5io.dtsi].
  - The pin-by-pin connector pinout is not in the local sources: check it against the CM5 IO
    board schematic.
  - So hardware sync needs a board change (a carrier routing FSIN to a header or to the camera
    module) or a flying lead from each module's FSIN pad.
- **One trigger for both sensors.** Route the FSIN of both sensors to either:
  - one RP1 GPIO driven by a timer; or
  - the VSYNC or strobe output of a master sensor, once its VSYNC-out mode is confirmed
    **[datasheet?]**.
  Keep the two traces about the same length. At these rates, any length mismatch matters far
  less than the trigger latency jitter.
- **Level shifting.** RP1 GPIO bank 0 is 3.3 V. If the module's FSIN is 1.8 V I/O
  **[datasheet?]**, use a unidirectional level shifter (e.g. a 74LVC1T45-class part), or an
  open-drain driver with a pull-up to the sensor's DOVDD. Never drive 3.3 V into a 1.8 V pin.
- **A trigger source on RP1.** Three blocks can generate it:
  - the hardware PWM (`rp1_pwm0` pwm@98000, `rp1_pwm1` pwm@9c000; `pwm-rp1.c`): a fixed-rate
    trigger with no CPU in the loop;
  - PIO (`rp1_pio` pio@178000; `rp1-pio.c`, `pwm-pio-rp1.c`): precise pulses, bursts, or a
    trigger gated by software;
  - a GPIO toggled from user space (Lemnos GPIO v2): simplest, but it carries scheduling jitter
    of tens of µs or more.

  In trigger mode the trigger sets the frame rate. Exposure plus readout must fit in the period.
- **Strobe for lighting.** Enable STROBE (0x3006 bit 3) to drive an IR illuminator only during
  exposure. That again needs the pin broken out, plus a driver transistor.

### The software side: a `[sync]` section (proposed, not implemented)

Sensor descriptions could gain an optional section that puts the sensor into external-sync mode.
This sketch uses the existing step syntax and values only from the driver:

```toml
[sync]
# FSIN snapshot trigger (slave): written after `init` and the mode's registers. This section's
# `stream_on` replaces the description's: here it stays in standby until FSIN rises.
external = [
    [0x0100, 0x00], [0x4f00, 0x01], [0x3030, 0x04], [0x303f, 0x01],
    [0x302c, 0x00], [0x302f, 0x7f], [0x3823, 0x00],
]
stream_on = []
# Strobe output (STROBE_FRAME_SPAN in units of (width + hblank) / 192 µs).
strobe = { enable = [[0x3006, 0x0c]], disable = [[0x3006, 0x04]], span = { address = 0x3925, bytes = 4 } }
```

`CaptureRequest` would select it (for example `NativeSync::External`), and the native backend
would then expect frames at the trigger rate rather than at the mode's frame length.

It is **not implemented** in this change. It is neither trivial nor safe yet:

- `SensorDescription` is `deny_unknown_fields`. A new section changes validation
  (`desc/validate.rs`) and the compiled postcard form that MCU builds read.
- The 0x4F00 conflict above is unresolved.
- Trigger mode changes how the native backend must treat frame timing: there are no frames until
  triggered, and the frame period is set by the trigger.
- Nothing here could be tested on a board with FSIN wired.

The section belongs with the sensor bring-up work once a carrier exposes FSIN.

## Expected sync quality

| Setup | Spread between cameras | Why |
|---|---|---|
| Free-running, software grouping (`Nearest`, same rate) | Up to half a frame period: ≤ 16.7 ms at 30 fps, ≤ 8.3 ms at 60 fps. It wanders as the phase drifts. | Each sensor runs from its own crystal. The phase is arbitrary at start-up and drifts by the clocks' ppm difference (`styx_sync_drift_ppm`): 20 ppm is 20 µs per second, so the phase walks a full period in about half an hour at 30 fps. The grouper pairs nearest frames, so the worst case is T/2. A tolerance below T/2 drops frames (`unmatched`) whenever the phase is unlucky. |
| Software grouping, cameras started together | Often a few ms at first, then the same drift | Start-up alignment does not hold without a shared clock. |
| Hardware FSIN trigger, both sensors on one trigger | Microseconds **[datasheet?]** for the exposures themselves | Exposure starts a fixed latency after the trigger edge. What Styx measures also includes timestamping: native timestamps are the receiver's frame-start interrupt, which adds interrupt latency (typically tens of µs on a loaded CM5, measure it). So `styx_sync_spread_ms` shows an upper bound, and `styx_sync_drift_ppm` should read ~0. |

To check a setup:

1. Run the grouper with `Partial` and a generous tolerance.
2. Read `styx_sync_spread_ms{quantile="0.99"}` and `styx_sync_drift_ppm`.
3. With hardware sync, set the tolerance to a few hundred µs. `unmatched` drops then flag lost
   triggers.

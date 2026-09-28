# Styx sensor bridge protocol (version 1)

The bridge (`styx_sensor_bridge.ko`) is a V4L2 subdevice that the CSI-2 receiver binds to as its
sensor. It knows nothing about the sensor and never touches the I²C bus. A userspace sensor
driver (Styx, `styx-kernel::bus::bridge`) drives the sensor over i2c-dev and talks to the
bridge through its subdev node (`/dev/v4l-subdevN`) with standard V4L2 ioctls. The C constants
are in `styx_sensor_bridge.h`; the Rust mirror is `crates/kernel/src/bus/bridge_sys.rs`.

## Device tree

A platform device node (not an I²C client, so address and bus stay free for i2c-dev):

| Property | Required | Meaning |
|---|---|---|
| `compatible = "styx,sensor-bridge"` | yes | |
| `port/endpoint` | yes | CSI-2 D-PHY endpoint linked to the receiver: `data-lanes`, `clock-lanes`, `link-frequencies` (u64 list), optional `clock-noncontinuous` |
| `styx,sensor-name` | no | Sensor name for userspace (default `sensor`); also names the subdev `"<name> <dev_name>"` |
| `styx,i2c-bus` | no | phandle of the I²C adapter the sensor sits on (published as the adapter number; probe defers until it exists) |
| `styx,i2c-address` | no | 7-bit sensor address, published only |
| `styx,mbus-codes` | no | u32 list of accepted media bus codes, first is the default (default: all Bayer and mono 8/10/12/14/16-bit codes) |
| `styx,max-size`, `styx,min-size` | no | `<width height>` limits (defaults 8192×8192 and 16×16) |
| `clocks` | no | sensor input clock, enabled with power |
| `styx,supply-names` + `<name>-supply` | no | regulators, enabled in list order with power (up to 8) |
| `rotation`, `orientation` | no | exposed as the standard read-only controls |

`dts/styx-sensor-bridge-cm5-overlay.dts` is the CM5 camera-port overlay with the HeliOS OV9782
wiring.

### sysfs (on the platform device, `/sys/class/video4linux/v4l-subdevN/device/`)

`sensor_name`, `i2c_bus` (adapter number or `-1`), `i2c_address` (`0x60`), `clock_frequency`
(Hz, `0` without a clock), `stream_state` (as `STYX_CID_STREAM_STATE`). The driver link is
`styx-sensor-bridge`.

## Pad format

One source pad (pad 0). `VIDIOC_SUBDEV_S_FMT` with a code from the accepted list (anything else
becomes the first code) and a size clamped to the limits; field `NONE`, colorspace `RAW`.
`VIDIOC_SUBDEV_ENUM_MBUS_CODE` lists the codes, `ENUM_FRAME_SIZE` gives one continuous range.
Setting the active format while streaming fails with `EBUSY`. `get_selection` reports
`NATIVE_SIZE`/`CROP_BOUNDS`/`CROP_DEFAULT` = max size, `CROP` = current size.
`get_mbus_config` reports CSI-2 D-PHY with the endpoint's lane count and clock mode.

Userspace must also set the matching format on the receiver's sink pad (the link validation
compares them), as with any sensor.

## Controls

| Control | Type | Access | Meaning |
|---|---|---|---|
| `V4L2_CID_LINK_FREQ` | integer menu | rw (locked while streaming) | Entries from `link-frequencies`; the receiver programs its D-PHY from the selected entry |
| `V4L2_CID_PIXEL_RATE` | int64 | rw (locked while streaming) | Pixels per second (read-only in the standard definition; the bridge makes it writable) |
| `V4L2_CID_HBLANK` | int | rw | Horizontal blanking, pixels (0..INT_MAX) |
| `V4L2_CID_VBLANK` | int | rw | Vertical blanking, lines (0..INT_MAX); may change while streaming |
| `STYX_CID_STREAM_ACK` = `0x00982800` | int64 | write (execute on write) | Acknowledges a stream event, see below |
| `STYX_CID_STREAM_STATE` = `0x00982801` | int | read-only, volatile | 0 idle, 1 starting, 2 streaming, 3 stopping |
| `STYX_CID_ACK_TIMEOUT_MS` = `0x00982802` | int | rw | 10..10000, default 1000: how long start/stop wait |
| `STYX_CID_POWER` = `0x00982803` | bool | rw | 1 enables the supplies (list order) then the clock; 0 disables them. Off fails with `EBUSY` unless idle |
| `STYX_CID_STREAM_SEQUENCE` = `0x00982804` | int | read-only, volatile | Sequence of the last stream event |

The bridge does not interpret the timing controls; they are for the receiver (`LINK_FREQ`) and
for Styx and other readers (`PIXEL_RATE`, blanking). Set them with one `VIDIOC_S_EXT_CTRLS`
(`which = V4L2_CTRL_WHICH_CUR_VAL`).

## Stream events

Subscribe with `VIDIOC_SUBSCRIBE_EVENT`, `type = STYX_BRIDGE_EVENT_STREAM`
(`V4L2_EVENT_PRIVATE_START + 0x5354` = `0x08005354`), `id = 0`. The queue holds 4 events. Wait
with `poll(POLLPRI)`, dequeue with `VIDIOC_DQEVENT`. The bridge counts subscribers (across all
open handles); closing the last subscribed handle unsubscribes it.

Payload (`struct styx_bridge_stream_event`, `v4l2_event.u.data`, 64 bytes, native endian):

| Offset | Field | |
|---|---|---|
| 0 | `u32 version` | 1 |
| 4 | `u32 action` | 1 = start, 2 = stop |
| 8 | `u32 sequence` | never 0; echo it in the ack |
| 12 | `u32 timeout_ms` | the wait the bridge applies to this event |
| 16 | `s64 link_freq` | Hz, selected `LINK_FREQ` entry |
| 24 | `s64 pixel_rate` | |
| 32 | `u32 code`, 36 `u32 width`, 40 `u32 height` | active pad format |
| 44 | `s32 hblank`, 48 `s32 vblank` | |
| 52 | `u32 data_lanes` | |
| 56 | `u32 flags` | bit 0: continuous clock |
| 60 | `u32 reserved` | 0 |

## Acknowledgement

Write `STYX_CID_STREAM_ACK` with `VIDIOC_S_EXT_CTRLS`:

```
value = (status << 32) | sequence        // STYX_BRIDGE_ACK(seq, status)
status = 0                               // done: the sensor started / stopped
status = errno (1..4095)                 // failed: the bridge returns -errno to the receiver
```

The write fails with `ESTALE` if the sequence is not the one the bridge is waiting for (it
already timed out, or it is a duplicate). Writing a value with sequence 0 is a no-op.

## Order of operations

`rp1-cfe` (Raspberry Pi 6.12) starts the stream in `cfe_start_streaming` once every enabled video
node is streaming: it reads `get_mbus_config` (lanes), reads `LINK_FREQ` to program the D-PHY,
opens the CSI-2 receiver (`csi2_open_rx` → D-PHY reset and enable), and only then calls the
sensor's `s_stream(1)`. On stop it calls `s_stream(0)` first and closes the receiver after. The
bridge implements `s_stream` (which `rp1-cfe` calls directly; later kernels that use
`v4l2_subdev_enable_streams` fall back to it).

Start:

1. Userspace, before `VIDIOC_STREAMON`: powers the sensor (`STYX_CID_POWER` = 1 for bridge-owned
   supplies/clock, GPIOs itself), waits its power-up time, loads the mode registers over I²C and
   leaves the sensor in software standby, **with its CSI-2 lanes in LP-11** (stop state). Sets
   the bridge pad format, `LINK_FREQ`, `PIXEL_RATE`, `HBLANK`, `VBLANK`, and the receiver's
   formats; subscribes to stream events (a thread or task other than the one calling STREAMON).
2. Something calls `VIDIOC_STREAMON` on the receiver's nodes. The receiver starts its D-PHY and
   calls the bridge's `s_stream(1)`.
3. The bridge: fails at once with `ENOTCONN` if nobody is subscribed and `EBUSY` if not idle;
   otherwise sets state `STARTING`, queues a start event and waits up to `timeout_ms`
   (killable).
4. Userspace dequeues the event, checks the format and timing in it, writes the stream-on
   register(s), and acknowledges with status 0 (or an errno if it failed).
5. The bridge returns 0 (state `STREAMING`, `LINK_FREQ` and `PIXEL_RATE` locked) or the error;
   STREAMON returns it.

Stop:

1. `VIDIOC_STREAMOFF`; the receiver calls `s_stream(0)` before closing its D-PHY.
2. The bridge sets `STOPPING`, queues a stop event and waits up to `timeout_ms`.
3. Userspace puts the sensor in standby (lanes back to LP-11) and acknowledges.
4. The bridge goes `IDLE` whatever happened (a stop cannot fail for the receiver); a missing or
   failed ack is logged.

## Errors and timeouts

| Case | Result |
|---|---|
| No subscriber at start | `s_stream(1)` → `-ENOTCONN` immediately |
| Last subscriber closes while the bridge waits | wait ends with `-ENOTCONN` |
| No ack within `timeout_ms` | start → `-ETIMEDOUT` (state back to idle); stop → idle, logged |
| Ack with status N | start → `-N`; stop → idle, logged |
| STREAMON caller killed while waiting | `-ERESTARTSYS`/killed; state back to idle |
| Late or duplicate ack | control write fails with `ESTALE` |
| Module unbound while waiting | wait ends with `-ENODEV` |
| Format change while streaming | `EBUSY`; `LINK_FREQ`/`PIXEL_RATE` writes `EBUSY` |

After a failed start userspace should put the sensor back in standby: the receiver has already
closed its side.

## Frame timing

The bridge sees no frames. Frame-start events come from the receiver's video nodes
(`V4L2_EVENT_FRAME_SYNC` on `rp1-cfe`), and buffer timestamps and sequence numbers from the
dequeued buffers. `STYX_CID_STREAM_STATE` and the sysfs `stream_state` expose whether the
receiver is streaming.

## Notes for the sensor side

- With `clock-noncontinuous` absent (HeliOS uses `clk-continuous`), the sensor drives a
  continuous HS clock once streaming starts; the receiver's D-PHY must already be enabled,
  which the order above guarantees. The sensor must not stream before the start event.
- Sensor I²C accesses go through i2c-dev; the bridge leaves the address unclaimed so
  `I2C_SLAVE` (never `I2C_SLAVE_FORCE`) succeeds.
- Supplies owned by the bridge (e.g. the CM5 `cam0_reg` regulator, a GPIO that the regulator
  driver holds) are only switchable through `STYX_CID_POWER`. Unused regulators are turned off
  by the kernel ~30 s after boot, so power the sensor through the bridge before use.

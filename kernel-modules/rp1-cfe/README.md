# rp1-cfe: the receiver driver with a mid-frame line event

This is Styx's build of the Raspberry Pi 5 / CM5 camera receiver driver: the downstream
`rp1_cfe` that the Raze image loads as `rp1-cfe-downstream`. It is the image's own driver plus
one event, which lets Styx start processing a frame before it has all arrived (see the slice
plan in `docs/native-stack/`).

The PiSP front end raises an ILINES interrupt per image output per frame, once the output has
written a set number of lines. The stock driver enables it and discards it. This build changes
three things:

- **Forwards the interrupt** to userspace as `STYX_CFE_EVENT_LINES`
  (`V4L2_EVENT_PRIVATE_START + 0x5354`). It is queued on the output's video node
  (`rp1-cfe-fe_image0` / `fe_image1`). The payload in `u.data` is
  `struct styx_cfe_lines_event { u32 sequence; u32 lines; u64 timestamp_ns; }`:
  - `sequence` is the frame's sequence, as its buffer will carry;
  - `lines` is the threshold the frame was queued with;
  - `timestamp_ns` is `CLOCK_MONOTONIC`, like the buffer timestamps.
- **Keeps the threshold** that the FE config sets (`ch[i].output.ilines`, 0 < ilines < height),
  so userspace picks the slice line. 0 keeps the driver's 3/4 of the height.
- **Allows subscribing** to the event on the two FE image nodes. Any file handle on the node can
  subscribe, so a monitor process works alongside a capture.

Everything else is unchanged: module name, uAPI and register programming.

| File | What |
|---|---|
| `*.c`, `*.h` | The driver. Git history has two commits: the unchanged import, then the Styx patch (`./build.sh diff`) |
| `Kbuild` | Out-of-tree build, object `rp1-cfe-downstream.ko` |
| `build.sh` | Builds against an image's own kernel tree. `verify` checks vermagic and the `struct module` size against a device |

## Provenance

`drivers/media/platform/raspberrypi/rp1_cfe/` of raspberrypi/linux `rpi-7.2.y` at `53679a578`
(7.2.9). It is byte-identical to the PhotonVision Raze image's Buildroot `linux-custom` tree.
`Makefile.stock` is the in-tree Makefile. GPL-2.0, Copyright Raspberry Pi Ltd; the changes are
GPL-2.0 too.

## Measured (CM5, Raze PhotonVision r42, OV9782 1280x800 RAW8 at 120 fps)

At the driver's default threshold (line 600 of 800), the event lands **4.777 ms after the
frame's start**, with p10–p90 4.777–4.778 ms and max 4.784 over 300 frames. That is 600 lines ×
7.58 µs plus about 0.23 ms through the receiver and the FE. Captures run unchanged with the module
(start of frame → delivery 7.80 ms).

## Dev box

With the camera's users stopped, the module can be swapped directly: unloading the receiver
tears the whole graph down, and the sensor driver stays loaded.

```sh
systemctl stop photonvision
rmmod rp1_cfe_downstream && insmod ./rp1-cfe-downstream.ko     # Styx's
rmmod rp1_cfe_downstream && modprobe rp1_cfe_downstream         # back to the image's
systemctl start photonvision
```

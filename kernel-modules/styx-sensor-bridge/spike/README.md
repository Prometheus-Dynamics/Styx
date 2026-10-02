# Native stack spike: OV9782 through the sensor bridge, at runtime

Raw OV9782 frames from `rp1-cfe` on the HeliOS CM5 (`root@helios`, kernel `6.12.47-v8-16k`),
with the sensor driven from userspace Rust over I²C and `styx_sensor_bridge.ko` standing in
for the sensor driver. Everything happens at runtime: nothing touches `/boot`, the A/B
squashfs images or the updater, and **a reboot restores everything**.

**Nothing here has been run on the device except the read-only parts** (`up.sh --check`,
`native-spike --dry-run`). Running `up.sh` needs the owner's approval: it stops
`helios-peripherals`, unbinds drivers, applies an overlay and loads a module.

## What changes on the device

| Step | Change | Undone by |
|---|---|---|
| 1 | `systemctl stop helios-peripherals` (it holds `/dev/media0`, `/dev/video*`) | `systemctl start` |
| 2 | unbind `ov9282` from `10-0060`, unbind `rp1-cfe` from `1f00110000.csi` | rebind |
| 3 | configfs overlay `styx-sensor-bridge` (`styx-sensor-bridge-cm5-runtime.dtbo`) | `rmdir` of the overlay |
| 4 | `insmod styx_sensor_bridge.ko` | `rmmod` |
| 5 | bind `rp1-cfe`; it finds the bridge as its sensor | unbind |

The overlay (`../dts/styx-sensor-bridge-cm5-runtime-overlay.dts`) goes on top of the live tree,
which has the HeliOS `ov9782-overlay` applied at boot on cam0 (`cam0,clk-continuous`):

1. `i2c_csi_dsi0` (`i2c@88000`, `/dev/i2c-10`) `ov9782@60`: `status = "disabled"`. The i2c
   core unregisters client `10-0060`, so `I2C_SLAVE` on address `0x60` works (it fails with
   `EBUSY` while any client exists there, bound or not).
2. `/styx-sensor-bridge-cam0`: the bridge, with the sensor node's supplies (`avdd` =
   `cam0_reg`, `dovdd`/`dvdd` = `cam_dummy_reg`), clock (`cam0_clk`, 24 MHz), 2 data lanes,
   400 MHz link, continuous clock, `styx,i2c-bus = <&i2c_csi_dsi0>`, address `0x60`, codes
   `SBGGR10_1X10`/`SBGGR8_1X8`, 1280x800 max.
3. `csi0` (`csi@110000`, `rp1-cfe`) `port/endpoint`: `remote-endpoint` → the bridge endpoint.
   The bridge endpoint has no `remote-endpoint` back: nothing reads it, and with it
   fw_devlink would make the bridge a consumer of the (unbound) csi0 device and defer its
   probe until `rp1-cfe` binds.

Removing the overlay restores all three; the `10-0060` client comes back and `ov9282`
probes it again.

## Files to copy to the device

Into one directory under `/tmp` (the device's `/tmp` is a tmpfs; `scp` does not work, use
`ssh ... 'cat > file'`):

```sh
D=/tmp/styx-spike
OUT=../linux-build-styx/out/6.12.47-v8-16k          # from build.sh
ssh root@helios "mkdir -p $D"
for f in up.sh down.sh spike-env.sh; do
    ssh root@helios "cat > $D/$f" < kernel-modules/styx-sensor-bridge/spike/$f
done
for f in styx_sensor_bridge.ko styx-sensor-bridge-cm5-runtime.dtbo; do
    ssh root@helios "cat > $D/$f" < $OUT/$f
done
ssh root@helios "cat > $D/native-spike && chmod +x $D/native-spike $D/*.sh" \
    < target/aarch64-unknown-linux-gnu/release/native-spike
ssh root@helios "cat > $D/ov9782.toml" < crates/sensor/sensors/ov9782.toml
```

Build: `./kernel-modules/styx-sensor-bridge/build.sh build` (module and both overlays), and
`cargo build --release --target aarch64-unknown-linux-gnu -p native-spike`.

## Sequence

```sh
cd /tmp/styx-spike
sh up.sh --check                               # read-only preflight and current state
./native-spike --dry-run --description ov9782.toml   # read-only
sh up.sh                                       # steps 1-5, then a dry run against the bridge
./native-spike --description ov9782.toml       # the spike (Ctrl-C cleans up)
sh down.sh                                     # undo
```

`up.sh` checks each step and stops at the first failure, printing how to undo. It is
idempotent: finished steps are skipped (with the overlay applied it leaves `rp1-cfe` alone).

`native-spike` (see `tools/native-spike`):

1. finds the bridge in sysfs, opens it, claims I²C `10-0060` with `I2C_SLAVE`;
2. subscribes to the bridge's stream events, runs the description's `power_up` (the bridge's
   `STYX_CID_POWER` switches `cam0_reg` and the clock; there are no reset/powerdown GPIOs),
   reads the chip id `0x300a` as one 2-byte burst and byte by byte (they must agree), writes
   `init` and the 1280x800 raw10 mode. Nothing before `stream_on` writes `0x0100 = 1`
   (checked, also in `--dry-run`), so the sensor stays in software standby, lanes in LP-11;
3. sets the bridge pad format (`SBGGR10_1X10` 1280x800) and timing (`LINK_FREQ` index of
   400 MHz, `PIXEL_RATE` 160 M, `HBLANK` 176, `VBLANK` 1022);
4. media graph: disables every other enabled mutable link (`csi2:4 -> pisp-fe:0`,
   `pisp-fe:2 -> fe_image0`, `pisp-fe:4 -> fe_stats`, `fe_config -> pisp-fe:1`; `rp1-cfe` waits
   for every node with an enabled link to stream), enables `csi2:4 -> rp1-cfe-csi2_ch0:0`, sets
   `csi2` pad 0 (propagates to pad 4) to the bridge format, sets `/dev/video0` to `pBAA`
   (the only format `rp1-cfe-csi2_ch0` offers for `SBGGR10_1X10`), stride 1600;
5. starts the acknowledgement thread (bridge start → `start_streaming`, stop →
   `stop_streaming`, acknowledged with the result), requests 4 MMAP buffers, subscribes to
   `FRAME_SYNC` on the video node, STREAMON (blocks until the start is acknowledged);
6. baseline: N frames, reports fps, sequence gaps, error buffers, mean level;
7. frame rates 30/60/120 through `Timing::frame_length_for_fps` and
   `SensorDriver::request(frame_duration)`, written at frame start by the scheduler; reports
   predicted vs measured fps and the first frame at the new period vs the predicted landing;
8. exposure at 30 fps: doubles then halves it (or the reverse in a bright scene) via
   `SensorDriver::request(exposure)` + `frame_start` on every frame-start event; prints per
   frame level and predicted exposure, and the frame the level moved vs the predicted landing
   frame;
9. saves one frame to `/tmp/native-spike-1280x800-pBAA-<seq>.raw` (as captured) and `.pgm`
   (2x2 cells averaged to 8-bit grey, 640x400);
10. STREAMOFF (the acknowledgement thread puts the sensor in standby), joins the thread, frees
    buffers, runs `power_down`, switches the bridge off. The same cleanup runs on every error
    and on the first Ctrl-C/SIGTERM (a second one exits at once).

Exit status 1 if a measured rate is more than 2% off, or an exposure change did not land on
the predicted frame.

## Verification (`native-spike` options)

`native-spike --help` lists them. Measured with them on the device (details in `ov9782.toml`):
the black frames were lost register writes (combined I²C transfers; `0x380a` read back 0, so
the sensor sent 32 lines and the rest of every buffer stayed 0); frame lengths above 4096 lines
stalled the sensor until `0x4f00 = 0x08`; delays are 2 frames for exposure and gain and 1 for
frame length; `0x3208` group hold is atomic (`0x3308` is not a group hold); the black level is
64 at 10 bits. `--kernel max:max,642:16` streams through the kernel driver (`ov9282` bound,
before `up.sh`) for comparison. `--embedded` needs the `-emb` overlay.

## Rollback

`sh down.sh` (best effort, every step runs even if one fails):

1. SIGINT to a running `native-spike`; 2. unbind `rp1-cfe`; 3. `rmmod styx_sensor_bridge`
(switches the bridge's supplies off); 4. `rmdir` the overlay; 5. bind `ov9282` to `10-0060` if
the i2c core did not already; 6. bind `rp1-cfe`; 7. `systemctl start helios-peripherals`.

If any step fails it says so: **reboot** (nothing here persists).

## Offline checks (host)

```sh
ssh root@helios 'cd /proc/device-tree && tar cf - .' | tar xf - -C live-fs   # read-only copy
./check-overlay.sh live-fs ../linux-build-styx/out/6.12.47-v8-16k/styx-sensor-bridge-cm5-runtime.dtbo
./test-check-overlay.sh live-fs
```

`check-overlay.sh` applies the overlay with `fdtoverlay` and checks the merged tree: bridge
node enabled; csi0 endpoint → bridge endpoint (and no back-reference); ov9782 client disabled;
bridge clock, supplies, lanes, link frequency and I²C bus equal the sensor node's; clock mode
the same at both ends. `test-check-overlay.sh` runs it on a HeliOS-shaped tree, on broken
variants (which must fail) and on the live copy.

## Known risks

- The overlay is verified offline with `fdtoverlay` (libfdt), not the kernel's resolver; the
  kernel's configfs path has extra rules (merged nodes must not carry phandles; this
  overlay's merged nodes have none) and warns about property updates on base nodes
  ("memory leak will occur if overlay removed"), which is expected.
- `cam0_reg` is a GPIO regulator with no start-up delay in the live tree; the spike waits
  5 ms (`--power-settle-ms`) after switching power on, on top of the description's 600 us.
- The OV9782 values come from the GPL `ov9782.c` driver (see `ov9782.toml`); group hold at
  `0x3308` and the 2-frame control delays are unverified. The exposure check is what tests
  them; a dark scene makes it inconclusive (it says so).
- If `rp1-cfe` is not unbound before the overlay is applied, it keeps its notifier for the
  old sensor node; `up.sh` always unbinds first.
- Removing the overlay after `rp1-cfe` was bound to the bridge logs `OF: ERROR: memory leak,
  expected refcount 1 instead of 2 ... /styx-sensor-bridge-cam0`. The reference is
  `rp1-cfe`'s: its probe adds the sensor node to its async notifier
  (`v4l2_async_nf_add_fwnode`, which takes a reference) and `cfe_remove` unregisters the
  notifier without `v4l2_async_nf_cleanup`, so the reference is never dropped. Measured on the
  device: overlay applied and removed with no module, with the module bound, and with the
  overlay removed under a bound bridge: no message; only after `rp1-cfe` has bound to the
  bridge. One node (a few hundred bytes) leaks per `up.sh`/`down.sh` cycle; nothing uses it
  afterwards. The fix belongs in `rp1-cfe` (`v4l2_async_nf_cleanup` after
  `v4l2_async_nf_unregister` in `cfe_remove`, as mainline has).


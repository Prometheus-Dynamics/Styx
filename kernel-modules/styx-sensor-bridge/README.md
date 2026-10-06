# styx-sensor-bridge

A generic Linux kernel module (GPL-2.0) that stands in for a camera sensor driver: it registers a
V4L2 subdevice bound to the CSI receiver through the device tree, while the sensor itself is
driven from userspace by Styx. Written once; no sensor-specific code, no I²C access. See
`docs/native-stack/README.md` and [`PROTOCOL.md`](PROTOCOL.md).

| File | What |
|---|---|
| `styx_sensor_bridge.c` | The module (platform driver, compatible `styx,sensor-bridge`) |
| `styx_sensor_bridge.h` | Userspace ABI: event id, payload, private controls |
| `PROTOCOL.md` | Device tree binding, controls, events, acknowledgement, start/stop order |
| `dts/styx-sensor-bridge-cm5-overlay.dts` | CM5/Pi 5 camera-port overlay with the HeliOS OV9782 wiring |
| `dts/styx-sensor-bridge-cm5-runtime-overlay.dts` | The same, applied at runtime (configfs) on top of a HeliOS tree booted with `ov9782-overlay` on cam0 (`-runtime-emb`: with the embedded data pad) |
| `dts/styx-cam0-i2c-fast-overlay.dts` | cam0's I²C bus at a faster clock |
| `spike/` | The first on-device spike: `up.sh`/`down.sh`, offline overlay checks, [`spike/README.md`](spike/README.md) |
| `install/` | Dev-box install: `install.sh` (boot overlay and module), `camera-mode.sh` (native / libcamera switch), `styx-bridge.service` (the runtime path, libcamera mode only) |
| `Kbuild`, `Makefile`, `Kconfig` | Out-of-tree and in-tree build glue |
| `build.sh` | Builds the `.ko` and `.dtbo`s for an exact device kernel (6.12 or 7.2), test-applies the overlays, checks vermagic/CRCs |
| `../kernel-env.sh` | Kernel selection shared with `../pispbe/build.sh` (`STYX_KERNEL=6.12\|7.2`) |
| `tools/check_crcs.py` | Compares a `Module.symvers` with the CRCs recorded in modules from the device |
| `buildroot/` | Buildroot linux extension (`linux-ext-styx-sensor-bridge.mk`, `Config.ext.in`), kernel-agnostic; optionally the PiSP back end driver |

## Kernels

One source for both kernels in the field; no version guards are needed (every API the module
uses is the same in both, and the platform driver's `remove()` already returns `void`).

| Kernel | Images | Release | Tree for `build.sh` |
|---|---|---|---|
| Raspberry Pi `stable_20250916` | HeliOS, Raze 1.0.x | `6.12.47-v8-16k` | `STYX_KERNEL=6.12` (default): the HeliOS Gaia Buildroot `linux-custom` |
| Raspberry Pi `rpi-7.2.y` | Raze 1.1.0 | `7.2.9-v8-16k` (`53679a5`) | `STYX_KERNEL=7.2`: an O= build at `../linux-rpi-7.2/build`, or the image's `linux-custom` via `KERNEL_TREE` |

Both are `bcm2712_defconfig` with 4K pages despite the `-v8-16k` release name: Buildroot sets
`CONFIG_ARM64_4K_PAGES` (its default `BR2_ARM64_PAGE_SIZE_4K`) over the defconfig's 16K, and
the Raze package adds `VIDEO_OV9282=m`. A 7.2 tree that matches the Raze 1.1.0 kernel's config
and compiler (the HeliOS Buildroot GCC 14.3):

```sh
git clone --depth 1 -b rpi-7.2.y https://github.com/raspberrypi/linux.git ../linux-rpi-7.2/linux
cd ../linux-rpi-7.2/linux
CC=<HeliOS Gaia output>/host/bin/aarch64-linux-
make O=../build ARCH=arm64 CROSS_COMPILE=$CC bcm2712_defconfig
scripts/config --file ../build/.config --enable ARM64_4K_PAGES --disable ARM64_16K_PAGES \
    --module VIDEO_OV9282
make O=../build ARCH=arm64 CROSS_COMPILE=$CC olddefconfig
# LOCALVERSION= keeps setlocalversion from appending "+" for a git tree (a tarball build has none)
make O=../build ARCH=arm64 CROSS_COMPILE=$CC LOCALVERSION= -j$(nproc) Image modules dtbs
```

Its `Module.symvers` CRCs only match a device whose kernel was built from the same commit,
config and compiler; for a device check build against that image's own tree.

On 7.2 the receiver module is `rp1-cfe-downstream` (same driver, name `rp1-cfe`, compatible
and video node names as 6.12's `rp1-cfe`; `verify` fetches the right one). Its sensor-facing
code is unchanged, including the failed-start oops behind `report_start_errors` (see
`docs/native-stack/README.md`).

## Building for a device image

Modules only load when vermagic and every imported symbol CRC (`CONFIG_MODVERSIONS`) match the
running kernel, so the module is built against the configured and built kernel tree that
produced the image: Buildroot's `output/build/linux-custom`, compiled with the same Buildroot
cross GCC. `build.sh` copies the external-module subset of that tree (headers, `.config`,
`Module.symvers`, host tools) with the kernel's own `scripts/package/install-extmod-build`, never
writing to it, into `../linux-build-styx/kbuild-<release>/` next to the repository. An O= tree
works too (its `source` link gives the source tree).

```sh
./build.sh prepare               # once per kernel build (slow on a busy disk)
./build.sh build                 # -> ../linux-build-styx/out/<release>/{styx_sensor_bridge.ko,*.dtbo}
DEVICE=root@helios ./build.sh verify   # vermagic + CRC comparison, read-only on the device
STYX_KERNEL=7.2 ./build.sh all   # the same for rpi-7.2.y
```

`build` also applies every overlay to the tree's `bcm2712-rpi-cm5-cm5io` and `bcm2712-rpi-5-b`
device trees with `fdtoverlay`, so a renamed label fails the build rather than the boot (the
firmware's `__overrides__`, e.g. `cam0`, are not evaluated there).

`STYX_KERNEL`, `KERNEL_TREE`, `KERNEL_SRC`, `BR_HOST`, `CROSS_COMPILE`, `KERNEL_RELEASE` and
`STYX_KBUILD_ROOT` override the defaults (see the script header and `../kernel-env.sh`). The
default 6.12 `KERNEL_TREE` is the Gaia build of `HeliOS-architecture-overhaul`
(`helios-full-cm5`), whose `Module.symvers` matches the device image
`6.12.47-v8-16k #1 SMP PREEMPT Thu May 7 00:00:13 PDT 2026`. (The older
`HeliOS/gaia/build/buildroot/output-cm5` tree has the same release string but different CRCs:
modules built against it would be rejected.)

For images built from now on, `buildroot/linux-ext-styx-sensor-bridge.mk` builds the module as
part of whatever kernel the image uses (6.12 or 7.2, e.g. in the Raze 1.1.0 device package's
external tree) instead, so it always matches.

## On the HeliOS CM5 dev box

The dev box boots as an image would (the shipping configuration): `config.txt` has
`dtoverlay=styx-sensor-bridge-cm5,cam0,clk-continuous` in place of
`dtoverlay=ov9782-overlay,cam0,clk-continuous`, the overlay is `overlays/styx-sensor-bridge-cm5.dtbo`
on the boot partition, and the module is `/lib/modules/$(uname -r)/updates/styx_sensor_bridge.ko`
with `depmod` run: udev loads it at boot by the bridge node's compatible, the bridge binds,
then `rp1-cfe` finds it as its sensor (video nodes, embedded data node). `ov9282` is not loaded.
The root file system is a squashfs with a persistent overlay (one per A/B slot), so
`/lib/modules/.../updates` survives reboots without touching the image; an update to the other
slot starts from a fresh overlay (rerun `install.sh` there).

```sh
./build.sh build                                    # module and overlays
install/install.sh [--reboot] [root@helios]         # under the device lock
ssh root@helios sh /usr/local/lib/styx-bridge/camera-mode.sh status
ssh root@helios sh /usr/local/lib/styx-bridge/camera-mode.sh libcamera   # ov9282 + libcamera, reboots
ssh root@helios sh /usr/local/lib/styx-bridge/camera-mode.sh native      # back, reboots
```

`camera-mode.sh` changes only the cam0 overlay line of `config.txt` (the first switch keeps the
original as `config.txt.pre-styx-bridge-boot`) and enables `helios-peripherals` at boot in
libcamera mode, disables it in native mode. In libcamera mode the runtime path still works
(`styx-bridge.service`: `spike/up.sh` with the runtime overlay, `down.sh` to hand the camera
back); in native mode it does not apply. Measured 2026-10-02: boot to the bridge bound 5.6 s,
`rp1-cfe` nodes registered at 6.0 s, `native-pipeline pisp` 30 and 60 fps with embedded data on
every frame, open to first frame 30 ms; libcamera mode and back each boot in about 20 s.

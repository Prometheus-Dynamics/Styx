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
| `Kbuild`, `Makefile`, `Kconfig` | Out-of-tree and in-tree build glue |
| `build.sh` | Builds the `.ko` and `.dtbo` for the exact HeliOS kernel and checks vermagic/CRCs |
| `tools/check_crcs.py` | Compares a `Module.symvers` with the CRCs recorded in modules from the device |
| `buildroot/` | Buildroot linux extension (`linux-ext-styx-sensor-bridge.mk`, `Config.ext.in`) |

## Building for the HeliOS CM5 image

Modules only load when vermagic and every imported symbol CRC (`CONFIG_MODVERSIONS`) match the
running kernel, so the module is built against the configured and built kernel tree that
produced the image: Buildroot's `output/build/linux-custom`, compiled with the same Buildroot
cross GCC. `build.sh` copies the external-module subset of that tree (headers, `.config`,
`Module.symvers`, host tools) with the kernel's own `scripts/package/install-extmod-build`, never
writing to it, into `../linux-build-styx/kbuild-<release>/` next to the repository.

```sh
./build.sh prepare               # once per kernel build (slow on a busy disk)
./build.sh build                 # -> ../linux-build-styx/out/<release>/{styx_sensor_bridge.ko,styx-sensor-bridge-cm5.dtbo}
DEVICE=root@helios ./build.sh verify   # vermagic + CRC comparison, read-only on the device
```

`KERNEL_TREE`, `BR_HOST`, `CROSS_COMPILE`, `KERNEL_RELEASE` and `STYX_KBUILD_ROOT` override the
defaults (see the script header). The default `KERNEL_TREE` is the Gaia build of
`HeliOS-architecture-overhaul` (`helios-full-cm5`), whose `Module.symvers` matches the device
image `6.12.47-v8-16k #1 SMP PREEMPT Thu May 7 00:00:13 PDT 2026`. (The older
`HeliOS/gaia/build/buildroot/output-cm5` tree has the same release string but different CRCs:
modules built against it would be rejected.)

For images built from now on, `buildroot/linux-ext-styx-sensor-bridge.mk` builds the module as
part of the kernel instead, so it always matches.

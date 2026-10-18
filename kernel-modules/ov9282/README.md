# ov9282: the OV9782 sensor driver with a centred 1280x480 window

This is Styx's build of the kernel's `ov9282` sensor driver, which drives the OV9281, OV9282 and
OV9782. It is the CM5 image kernel's own driver plus one mode:

- **1280x480, centred on the array.** It covers array rows 168..647, which are rows 160..639 of
  the 1280x800 frame.
- **Full pitch, flips off**, like the 1280x800 mode.
- **Fewer lines read.** The sensor reads rows 160..655 only, 40% fewer lines than 1280x800.

The module name and object are the same as the in-tree driver (`ov9282.ko`, module `ov9282`). An
image installs it to `/lib/modules/<release>/updates/`, where it takes precedence over the
kernel's own copy. The kernel itself is not changed, and images that use libcamera keep the
stock driver.

| File | What |
|---|---|
| `ov9282.c` | The driver. Git history has three separate commits: the stock import, the image patches, and the Styx mode |
| `Kbuild` | Out-of-tree build, object `ov9282.ko` |
| `build.sh` | Builds against an image's own kernel tree. `verify` checks vermagic and the `struct module` size against a device |
| `install/swap.sh` | Dev box only: swaps the running driver (stock ↔ Styx) |
| `buildroot/` | A `BR2_EXTERNAL` tree (`STYX_KMODS`) with the `styx-ov9282` package, `styx-rp1-cfe` (`kernel-modules/rp1-cfe`) and `styx-pispbe` (`kernel-modules/pispbe`), for Styx images |

## Provenance

1. **Stock.** `drivers/media/i2c/ov9282.c` of raspberrypi/linux `rpi-7.2.y` at `53679a578`
   (7.2.9), sha256 `a3d85f3e…1c23`.
2. **Image patches.** The CM5 device package's `buildroot-external/linux/ov9782`
   (commit `8eb2f29`): `0001-media-i2c-ov9282-add-ov9782-variant-draft.patch` and
   `0002-media-i2c-ov9282-keep-control-defaults-inside-ranges.patch`.
   - The result is byte-identical to the CM5 image kernel's `ov9282.c`: sha256
     `2e148740…c25c9c`, checked against the device image's Buildroot `linux-custom`.
   - The Buildroot package refuses any other base.
3. **Styx.** `MODE_1280_480` and `mode_1280x480_regs` (`./build.sh diff` prints it).
   - Window registers: rows 160..655 (0x3802..0x3807); output 1280x480 (0x3808..0x380b); ISP
     offset 8,8.
   - Flips off (0x3820/0x3821 = 0x40/0x00).
   - BLC lines as in 1280x800 (0x4008/0x4009 = 4/11).
   - Minimum VBLANK 41, as for 1280x720.
   - The reported crop is left 8, top 168, 1280x480.

Licence: GPL-2.0 (SPDX header kept), Copyright the original authors. The changes are GPL-2.0
too.

## Measured (CM5, OV9782 1280x800 RAW8, Styx 54b1c91: compressed TDN)

- **Start of frame → delivery** (processed, 8 ms exposure): 4.85 ms at 1280x480, against 7.79 ms
  at 1280x800 (−2.94 ms).
- **Frame rate.** 200 fps runs (frame 4997 µs, 3996 µs exposure applied). The ceiling at VBLANK
  41 is about 210 fps.
- **Black level.** The sensor's own black-level calibration holds without the top dark rows.
  Raw per-channel means on the same array rows, window against 1280x800:
  - within 0.03 DN at 20 µs (B 64.86 vs 64.85);
  - −0.33 DN (0.2%) at 8 ms, with a flat row profile.
  - The 1280x800 mode of this module is identical to stock.
- **Tags.** AprilTag C on the tag scene: every tag inside the band detected, with jitter as at
  1280x800 (0.03–0.05 px).

## Dev box

The device image's root is read-only (erofs), so `/lib/modules` cannot change there. To try
the module, swap it at runtime as root, under the device lock, with the camera service stopped:

```sh
./install/swap.sh styx ./ov9282.ko   # unbind rp1-cfe, rmmod, insmod, bind rp1-cfe
./install/swap.sh stock              # back to the image's own driver
```

Order matters. Unbinding the receiver (rp1-cfe) first tears the camera media graph down, and
binding it again builds the graph around whichever driver is loaded. Re-binding only the sensor
into a live graph does not work with `rp1-cfe-downstream`: you get
`media_device_register_entity` warnings and no sensor subdev until a reboot.

Build against the image's own kernel tree. A separately configured tree can match vermagic and
still differ in `struct module`; the image kernel has tracing off, for example. The kernel then
refuses the module ("section size must match the kernel's"). `DEVICE=root@host ./build.sh all`
checks both.

## Images (Styx variants only)

Add the external tree to the Styx layer and enable the package:

```toml
# image builder's layer config: a layer's external_tree replaces the earlier one,
# so list the base's trees too
external_tree = "<the base's trees>:@source:styx/kernel-modules/ov9282/buildroot"
# defconfig fragment
BR2_PACKAGE_STYX_OV9282=y
BR2_PACKAGE_STYX_RP1_CFE=y   # the receiver with the line event (kernel-modules/rp1-cfe)
BR2_PACKAGE_STYX_PISPBE=y    # the back end with the cheaper per-job config write (kernel-modules/pispbe)
```

How the package behaves:
- Built with Buildroot's `kernel-module` infrastructure against the image's own `linux` build
  (its `.config`, `Module.symvers` and toolchain).
- Installed with `INSTALL_MOD_DIR=updates`. Buildroot's `depmod` indexes it, and `updates/` comes
  before the kernel's own `ov9282.ko`.
- The kernel image and every other module are unchanged.
- If the kernel's `ov9282.c` is not the base above, the build stops with a message to re-import
  the driver, so a kernel bump never ships a driver made for another kernel.

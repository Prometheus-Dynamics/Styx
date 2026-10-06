# pispbe: the PiSP back end driver with a cheaper per-job config write

A patched build of the Raspberry Pi PiSP back end driver (`pisp_be`, GPL-2.0) that replaces the
image's module (Raspberry Pi 6.12 for HeliOS / Raze 1.0.x, rpi-7.2.y for Raze 1.1.0). Same module name, same uAPI, same hardware programming; the hardware sees the
same register values for every job. It only writes them faster: per job the stock driver spent
117 µs of CPU in `pispbe_schedule()`, this one 7-8 µs. See
[pisp.md](../../docs/native-stack/pisp.md#back-end-per-job) and
[pipeline.md](../../docs/native-stack/pipeline.md#the-back-end-drivers-config-write) for the
measurements.

| File | What |
|---|---|
| `pisp_be.c`, `pisp_be_formats.h` | The driver. Imported unchanged (own commit) from rpi-7.2.y, then patched |
| `Kbuild` | Out-of-tree build, object `pisp-be.ko` like the in-tree Makefile |
| `build.sh` | Builds against the exact image kernel (vermagic and symbol CRCs checked), `diff` prints the patch |
| `install/install.sh`, `install/uninstall.sh` | Dev-box override in `/lib/modules/<release>/updates/` |

## Provenance

- Source: `drivers/media/platform/raspberrypi/pisp_be/{pisp_be.c,pisp_be_formats.h}` of
  raspberrypi/linux branch `rpi-7.2.y` at commit `53679a578` ("misc: rp1-pio: Minor cyclic DMA
  error path fixes", `make kernelversion` 7.2.9), the kernel of Raze 1.1.0.
  sha256 of the originals: `pisp_be.c` 6d36c1076af7276aeb5b3f5f9d8f3f88ee6a3deb16477d1e3a977ca2459adbce,
  `pisp_be_formats.h` 2315e82cb18e89020e83220d3e3b7b13b64818ade5a70603021ea21dce635835
  (`pisp_be_formats.h` is unchanged). The directory's `pisp_be_config.h` is not used by the
  driver (it includes the uAPI header) and is not imported.
- Changes upstream since the first import (`stable_20250916`, 6.12.47: `pisp_be.c`
  6a7dee22…d8e5, `pisp_be_formats.h` 8f462bfe…db89): the NV12MT_COL128 and NV12MT_10_COL128
  output formats (a `pixel_grouping` field for the 10-bit one's 3-pixels-per-word line size),
  `.remove_new` renamed `.remove`, and `pm_runtime_mark_last_busy()` dropped before
  `pm_runtime_put_autosuspend()` at the end of probe (7.x marks it there itself). The Styx
  patch applied to the new source unchanged (offset one line).
- Both kernels: the same source builds W=1 clean against 6.12.47-v8-16k and 7.2.9-v8-16k.
  On 6.12 it adds the two COL128 formats and, after probe, starts the autosuspend delay from
  the last earlier busy mark; nothing in the per-job path differs.
- Licence: GPL-2.0 (SPDX header kept), Copyright (c) 2021-2024 Raspberry Pi Limited; the changes
  are GPL-2.0 too. Like the bridge module it lives apart from the MIT/Apache Rust crates.
- `git log -- kernel-modules/pispbe/pisp_be.c` shows each import and the patch separately;
  `STYX_KERNEL=7.2 ./build.sh diff` prints the patch against the kernel tree.

## What changed

The back end takes its configuration from registers, not from memory: per job the driver
writes the buffer addresses and enables, then all of `struct pisp_be_config` from
`global.bayer_order` on (1589 32-bit words, 6.4 KB) with `writel()`, reads the 26 address
registers back, and queues the tile list (the only part the hardware reads by DMA). Measured on
the CM5 (Styx's PiSP path; in-kernel timestamps per phase over 200 jobs, `function_graph` over
150):

| per job | stock | patched |
|---|---|---|
| `pispbe_schedule()` (the inlined `pispbe_queue_job()`) | 117.1 µs | 7.2 µs median, 8.2 mean |
| of which the 1589 config words | 113.8 µs (71.6 ns per `writel()`) | 4.8-5.4 µs (compare all, write the ~250-300 that changed, relaxed) |
| same, all 1589 words with `writel_relaxed()` | — | 21.6 µs (13.6 ns per write) |
| addresses + enables (28 writes) / readback of the 26 address registers | 1.9 / 1.9 µs | relaxed / 1.7-1.8 µs |
| config `buf_prepare` (copy + validation) | 2.3 µs | 1.5 µs |

1. **No barrier per register write.** `writel()` is `dma_wmb()` (`dmb oshst`) plus the store;
   the barrier made each write wait for the previous one to drain, 71.6 ns per word. Writes to
   the same device (Device-nGnRE) are not reordered among themselves, so the config, address
   and enable writes use `writel_relaxed()`; the tile pointer and the control register that
   queues the job keep `writel()`, whose barrier orders all of them and the tile list in memory
   before the job starts.
2. **Only the words that changed.** A per-device shadow of the last values written; words equal
   to it are skipped. The configuration registers keep their values between jobs (checked: an
   instrumented build read all 1589 back before every job for 200 jobs of the live pipeline, 0
   differences). The shadow is dropped when the clock is gated (runtime suspend/resume) and
   when the address readback fails, so the next job writes everything. Both node groups share
   it (it tracks the hardware, not a client). Module parameter `skip_unchanged_config` (default
   `Y`, writable at runtime) turns it off.
3. **The config is read from cached memory.** The stock driver copied the whole
   `pisp_be_tiles_config` (16.7 KB) into its DMA-coherent (uncached) buffer and read the config
   back from there word by word (~130 ns per uncached load when not hidden behind the MMIO
   writes; 4 µs even as a `memcpy`). Now `buf_prepare` copies the config part into a per-buffer
   `kmalloc` copy (allocated at the first `buf_prepare`, freed in the new `buf_cleanup`),
   validates that copy, and copies only the validated number of tiles into the coherent buffer
   the hardware reads them from. The job uses the copy.

Not changed: the per-job `kzalloc` of the job descriptor (~0.2 µs) and the address readback.

### Output equality

`native-pipeline be-replay` with a synthetic 1280x800 Bayer frame and a sequence of 19 configs
(Styx's first and last config of a live run and variants: WBG, gamma, CCM, LSC table,
sharpening, LSC and sharpening blocks disabled and enabled again, repeats, in an order that
changes and restores blocks): every NV12 output identical (md5) between the stock driver (twice),
the patched driver with and without `skip_unchanged_config`, after loading by `insmod` and
after booting with it. libcamera (`helios-peripherals`, `styx-compare --backend libcamera`) runs
on it unchanged.

## Build, install, uninstall

```sh
../styx-sensor-bridge/build.sh prepare         # once per kernel build (shared kbuild subset)
DEVICE=root@helios ./build.sh all              # -> target/kernel-modules/6.12.47-v8-16k/pisp-be.ko
                                               #    vermagic + imported symbol CRCs vs the device
STYX_KERNEL=7.2 ../styx-sensor-bridge/build.sh prepare
STYX_KERNEL=7.2 ./build.sh all                 # -> target/kernel-modules/7.2.9-v8-16k/pisp-be.ko
# under the device lock (scripts/with-device-lock.sh):
install/install.sh --reboot root@helios        # updates/pisp-be.ko + depmod, reboot into it
install/install.sh --reload root@helios        # or swap the running module (no back end users:
                                               # nothing streaming, helios-peripherals stopped)
install/uninstall.sh --reboot root@helios      # back to the image's module
```

`install.sh` takes the module of `$KERNEL_RELEASE` (default 6.12.47-v8-16k) or an explicit path
as its second argument, and refuses a module whose vermagic differs from the device's.

Check: `modinfo -n pisp_be` is `/lib/modules/<release>/updates/pisp-be.ko` and
`/sys/module/pisp_be/parameters/skip_unchanged_config` exists. The override lives on the root
overlay's upper layer, like the bridge module: an update to the other A/B slot starts without it.
For images, the driver belongs in the kernel build rather than an out-of-tree module:
`../styx-sensor-bridge/buildroot/linux-ext-styx-sensor-bridge.mk` has an optional
`BR2_LINUX_KERNEL_EXT_STYX_PISPBE` that replaces the kernel's `pisp_be.c` when it is the stock
file this one was made from (sha256 above) and otherwise keeps the kernel's driver.

## Upstreaming

Upstreamable as three small patches to `drivers/media/platform/raspberrypi/pisp_be/pisp_be.c`
(the driver is in mainline since 6.11; check its current `pispbe_queue_job()` before sending).
Draft:

> **media: raspberrypi: pisp_be: Write the job configuration with relaxed MMIO accessors**
>
> Every job writes the ~1600-word back end configuration with writel(), whose barrier makes
> each write wait for the previous one: 114 us of CPU per job on a BCM2712. The writes go to
> one device and are not reordered among themselves; only the final control-register write
> that queues the job needs to be ordered after them and after the tile list in memory, which
> its writel() already does. Use writel_relaxed() for the configuration, address and enable
> registers (22 us per job).
>
> **media: raspberrypi: pisp_be: Read the configuration from a cached copy**
>
> The configuration was copied into the DMA-coherent buffer and read back from it word by word
> although the hardware reads only the tiles from memory. Keep the validated configuration in
> a cached per-buffer copy and copy only the validated tiles into the coherent buffer.
>
> **media: raspberrypi: pisp_be: Only write configuration words that changed**
>
> The configuration registers keep their values between jobs and most of the configuration
> does not change from frame to frame (about 300 of 1589 words in a typical stream). Keep a
> shadow of the values last written and skip equal words; drop the shadow on runtime suspend
> and resume and when the register readback fails. 117 us -> 7 us per job in total.

Open question for Raspberry Pi before sending the third: whether the configuration registers
(the readback check calls them "ISP RAMs") are guaranteed to hold their values across jobs
and across `PISP_BE_CONTROL_COPY_CONFIG`, and whether anything other than the clock gating
handled here can lose them. The first two are independent of that.

# Where a build's target lives

A worktree's target is `<area>/target-sv/<worktree>` (`scripts/area.sh`). On btrfs, `target-sv` is
a subvolume of its own. This page is the measurement behind that choice, and how to measure it
again.

## The measurement (2026-11-24)

The change that hurts most: a one-line edit to styx-frame, then every all-features test binary
rebuilt (incremental on), and the edit taken back out and rebuilt again. Three rounds, the order
of the targets rotated each round, six builds per target. The development PC was at load 90–137
from other work the whole time; the build ran on 26 cores with 13 jobs.

| Target | Median wall | Range | CPU-s per build |
|---|---|---|---|
| NVMe btrfs (zstd), in the filesystem's top subvolume beside every other project's target | 62.7 s | 42.6–94.5 | ~75 |
| The same, with compression off for the directory (`chattr +m`) | 55.0 s | 18.8–120.8 | ~75 |
| A btrfs subvolume of its own on the same disk | 18.9 s | 10.0–21.6 | ~75 |
| tmpfs (`/dev/shm`) | 11.8 s | 9.3–14.6 | ~75 |

Every target used the same CPU. The difference is waiting. The build's threads, sampled every
0.2 s (summed over the six builds; D is an uninterruptible wait in the kernel):

| Wait | Top subvolume | No compression | Own subvolume |
|---|---|---|---|
| D `btrfs_tree_read_lock_nested` | 669 | 539 | 3 |
| D `btrfs_tree_lock_nested` | 125 | 97 | 1 |
| D `filename_unlinkat` | 313 | 271 | 12 |

The compilers queued on the lock of the filesystem tree they shared with every other project's
build. That wait is a lock, not I/O, so the I/O pressure counters (`/proc/pressure/io`) showed
almost none of it (0.1–2.3 s of stall per build). Compression was not the cause. A subvolume has
a filesystem tree of its own, and the waits went with it.

tmpfs is faster still, but it costs about 8.5 GiB of RAM per worktree target, is gone after a
reboot, and a target restored into it from a seed is a copy, not a reflink (119 s for Styx's
seed). The subvolume needs no RAM, keeps its contents across a reboot, and takes seeds by reflink.

## Measuring it again

```
scripts/area.sh setup
STYX_AB_WAITS=1 scripts/ab-target.sh 3 <target> <target> ...
```

Each target must be warm first (one full `cargo test --workspace --all-features --no-run` in it).
`scripts/ab-target.sh` prints wall, CPU-s, load and I/O stall per build, and with
`STYX_AB_WAITS=1` what the build's threads waited on (`scripts/dstate-sample.sh`).

A target moves by being built again in the new place: a built target names its own path, so a
copied or renamed one is a different target to cargo.

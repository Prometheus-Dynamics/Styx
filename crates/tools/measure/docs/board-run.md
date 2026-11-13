# Board run: the raw path, measured from Styx's side

This is Styx's part of the raw-path run on a CM5 Raze: one native camera, the service serving it,
the software ISP run downstream by `styx-measure` (no hardware ISP), and the measurements. Lemnos
starts `lemnosd` and owns the camera's hardware; this file starts nothing of Lemnos's.

Every board-specific value is a placeholder `<LIKE_THIS>`. Fill each one from the final board
runbook (`raze-camera-runbook.md`, which is a draft and will change). Do not guess a value.

## What runs, and what is ours

| Process | Built from | Started by | Stopped by |
|---|---|---|---|
| `lemnosd` | Lemnos, `<LEMNOS_REV>` | this run (section 2, step 1) | its PID |
| `styx-service` | this PR, commit `<STYX_COMMIT>` | this run (step 3) | its PID |
| `styx-measure read` | this PR, commit `<STYX_COMMIT>` | this run (step 5), exits itself at its frame count | exits; its PID if it hangs |

Binaries: `/mnt/ssd-samsung/styx-targets/h1-kit/` (see its README.txt and SHA256SUMS). Check the
hashes on the board before the run.

Stop only processes this run started, by the PIDs it recorded. Never stop by name (no `pkill`,
no `killall`): the board runs other services (helios-engine and the rest) that this run does not own.

## 1. Preconditions (from the runbook)

- Board locks held as the runbook says: `flock` on `<BOARD_LOCK_FILE>` (the shared lock; see the
  runbook for the path, it is `/run/lock/raze-gpu.lock` in the draft).
- The product camera holders stopped and the stock drivers unbound, as the runbook's section 4
  (`<TAKE_STEPS_SECTION>`). Module state as the runbook says: `lemnos.ko` loaded with
  `<INSMOD_LINE>`.
- `lemnosd`'s board file `<BOARD_FILE>` passes `lemnosd --check` (exit 0).
- Record the board state before the run: `<STATE_CHECK_COMMAND>` output.

## 2. Start order

1. `lemnosd` (as the runbook's section 5 says, with `--user <LEMNOS_USER>`): `<LEMNOSD_START_COMMAND>`
   Record its PID: `<LEMNOSD_PID>`. Its socket is `<LEMNOS_SOCKET>`.
2. Check it: `lemnos-ctl list` (or `lemnos-ctl --socket <LEMNOS_SOCKET> list`). The camera line for
   `<SENSOR_DEVICE_ID>` must read `running`. Its health may be `degraded` (the runbook records this as
   expected); frames still flow.
3. `styx-service <SERVICE_CONFIG>` (the config is in section 3). Record its PID: `<SERVICE_PID>`.
   It prints `styx-service: control socket <CONTROL_SOCKET>`.
4. `styx-measure status --control <CONTROL_SOCKET>` until the camera line reads
   `state=streaming publish=publishing`. Keep the answer for the run log.

## 3. Service config (one native camera, no hardware ISP)

Save as `<SERVICE_CONFIG>`:

```toml
# One native camera on the raw path. No hardware ISP: the software ISP runs in styx-measure.
socket_dir = "<SOCKET_DIR>"
lemnos_socket = "<LEMNOS_SOCKET>"
cameras = ["lemnos:<SENSOR_DEVICE_ID>"]
scan_ms = 2000
backoff_first_ms = 250
backoff_cap_ms = 5000

[camera."lemnos:<SENSOR_DEVICE_ID>"]
width = <WIDTH>          # the mode's width, at least (the chosen mode is the sensor's)
height = <HEIGHT>        # the mode's height, at least
fps_milli = <FPS_MILLI>  # 120000 for 120.00, 60000 for 60.00: the asked rate, as the backend's request
exposure_us = <EXPOSURE_US>
gain_milli = <GAIN_MILLI>
```

No `force`, no `format` (a native camera takes its format from the mode), no `tuning_dirs` effect (the
3A controller is not wired in this service), no `auto_off_after` (a camera that drops comes back).

The camera's frame socket is `<SOCKET_DIR>/cameras/<CAMERA_SOCKET_NAME>`. The service's own rule
computes its name from the id (`camera_socket` in `styx-service`); the `list` answer's `socket=` field
gives it exactly, so copy it from there: `<CAMERA_SOCKET>`.

## 4. Sensor modes and asked rates

The sensor has two modes the run measures. Their figures are the runbook's (`section 6`), to be
checked against the final runbook:

| Run name | Sensor mode | Asked rate | Expected line time, frame length | Source |
|---|---|---|---|---|
| fast | `<FAST_MODE_INDEX>` | 120.00 | `<FAST_LINES>` lines, continuous clock | runbook section 6 |
| slow | `<SLOW_MODE_INDEX>` | 60.00 | `<SLOW_LINES>` lines | runbook section 6 |

Run the 9000-frame measurement for each mode at each asked rate, so four runs in all: fast at 120.00
and at 60.00, slow at 120.00 and at 60.00. The runbook's figure for a mode is the expected rate; the
measured rate is what the run reports, and the two are compared, never assumed equal.

## 5. The measure commands

`<CAMERA_SOCKET>` and `<CONTROL_SOCKET>` come from step 2 and section 3. `<RUN_DIR>` is a directory
for this run's files (create it; the reports go there). Run each one as one command, in the order
listed; each exits when it has 9000 frames.

```sh
# fast mode, asked 120.00
styx-measure read --socket <CAMERA_SOCKET> --control <CONTROL_SOCKET> \
  --frames 9000 --asked-fps 120.00 --service-pid <SERVICE_PID> \
  --soft-isp NV12 --report <RUN_DIR>/fast-120.json | tee <RUN_DIR>/fast-120.txt

# fast mode, asked 60.00 (the service must be restarted with fps_milli = 60000 for this run)
styx-measure read --socket <CAMERA_SOCKET> --control <CONTROL_SOCKET> \
  --frames 9000 --asked-fps 60.00 --service-pid <SERVICE_PID> \
  --soft-isp NV12 --report <RUN_DIR>/fast-60.json | tee <RUN_DIR>/fast-60.txt

# slow mode, asked 120.00 and 60.00: same commands with the slow mode's config (restart the service
# for each change of config, and record the new <SERVICE_PID> and <CAMERA_SOCKET>)
styx-measure read ... --frames 9000 --asked-fps 120.00 ... --report <RUN_DIR>/slow-120.json
styx-measure read ... --frames 9000 --asked-fps 60.00  ... --report <RUN_DIR>/slow-60.json
```

The `--soft-isp NV12` option runs the software ISP on every frame into a picture pool; its time per
frame is in the report (`isp.time_ns`). A transport-only figure (no ISP) is the same command without
`--soft-isp`; run it once per mode if the CPU figures are wanted without the ISP's share.

Each command prints a short table and writes the JSON report. The JSON is the record; the table is for
the log.

## 6. What to record

For each run (all from the files above; nothing typed from memory):

- the report JSON and the table (`<RUN_DIR>/*.json`, `*.txt`);
- `frames.delivered` (must be 9000), `frames.skipped_by_ring`, `frames.gaps`, `frames.largest_gap`,
  `frames.duplicates`, `frames.out_of_order`, `frames.metadata_refused`;
- `rate.measured_fps`, `rate.interval_min_ns`, `rate.interval_median_ns`, `rate.interval_max_ns`,
  next to `asked.fps_milli` and the mode; `rate.sensor_fps` is `null` at this commit (the status and
  the metadata carry no rate), and that is recorded as it is;
- `latency_ns` min, median, p99, max (capture to reader, `CLOCK_MONOTONIC`);
- `copies.this_process_total` (must be 0) and the service's copies, which the status does not carry;
- `cpu.reader_ns_per_frame`, `cpu.service_ns_per_frame`, `cpu.service_total_ns`;
- `memory`: RSS and HWM at start and end, for the reader and the service;
- `isp`: output, time min/median/p99/max, `pool.misses` (must be 0), `errors` (must be 0);
- `service_camera.start` and `.end`: state, frames, skipped, last_id, publish, health (from `list`);
- the `lemnos-ctl list` answer at the end; the `styx-measure status` answer at the end;
- the sha256 of each binary used (`sha256sum`), the kernel release (`uname -r`), and
  `lsmod | grep lemnos`.

## 7. Stop

1. Each `styx-measure read` exits by itself at 9000 frames. If one does not, stop it by its PID.
2. Stop `styx-service` by its PID (`<SERVICE_PID>`), with `kill -TERM <SERVICE_PID>`.
3. Stop `lemnosd` by its PID (`<LEMNOSD_PID>`), with `kill -TERM <LEMNOSD_PID>`.
4. Then the runbook's hand-back (`<HANDBACK_SECTION>`): the stock drivers are restored by its steps,
   in its order. This run does not restore anything on its own.
5. Check the state checklist again (`<STATE_CHECK_COMMAND>`) and record it.

Only PIDs this run started are stopped. A PID that was not recorded is not touched.

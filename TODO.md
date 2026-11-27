# Styx `next`: the PR list

Every PR has a brief in `docs/prs/`. `python3 scripts/pr-status.py` shows what is ready.
Needs named `X<n>` are outside blocks, listed in `docs/design.md`.

## A. Foundation

The frame type, formats, kernels, the Lemnos client and one simulated camera. Everything else builds on these.

- [x] [A1](docs/prs/A1.md) FrameLease, slimmed (wave 1; needs nothing)
- [x] [A2](docs/prs/A2.md) Formats (wave 1; needs nothing)
- [x] [A3](docs/prs/A3.md) SIMD kernels (wave 2; needs A1, A2)
- [x] [A4](docs/prs/A4.md) Controls and metadata (wave 2; needs A1)
- [x] [A5](docs/prs/A5.md) Lemnos client for cameras (wave 2; needs A1, X1)
- [x] [A6](docs/prs/A6.md) One simulated camera (wave 3; needs A5)
- [x] [A7](docs/prs/A7.md) no_std check (wave 2; needs A1, A2)

## B. Cameras

Every way a camera comes up: native on Lemnos, libcamera, V4L2, and the choice between them.

- [x] [B1](docs/prs/B1.md) Native: find and assemble a camera (wave 3; needs A5)
- [x] [B2](docs/prs/B2.md) Native: modes and start order (wave 4; needs B1)
- [x] [B3](docs/prs/B3.md) Native: controls on frame identities (wave 5; needs B2, A4)
- [x] [B4](docs/prs/B4.md) Native USB cameras (wave 3; needs A5)
- [x] [B5](docs/prs/B5.md) libcamera backend (wave 3; needs A1, A4)
- [x] [B6](docs/prs/B6.md) V4L2 backend, on lemnos-v4l2 (wave 3; needs A1, A4, X6)
- [x] [B7](docs/prs/B7.md) Choosing a backend (wave 4; needs B1, B5, B6)
- [x] [B8](docs/prs/B8.md) Hotplug (wave 5; needs B7, X6)

## C. 3A and tuning

Making the image good on the native path. Tuning ships with Styx.

- [x] [C1](docs/prs/C1.md) 3A algorithms (wave 3; needs A4)
- [x] [C2](docs/prs/C2.md) One controller (wave 4; needs C1)
- [x] [C3](docs/prs/C3.md) Tuning files (wave 4; needs C1)
- [x] [C4](docs/prs/C4.md) Autofocus (wave 5; needs C2, A5)

## D. ISPs

Turning 3A results into ISP jobs, and the software and GPU ISPs for chips without one.

- [x] [D1](docs/prs/D1.md) Neutral ISP settings (wave 5; needs C2)
- [x] [D2](docs/prs/D2.md) PiSP, slices included (wave 6; needs D1, A5, X6)
- [x] [D3](docs/prs/D3.md) rkisp (wave 6; needs D1, A5, X6)
- [x] [D4](docs/prs/D4.md) K230 ISP (wave 6; needs D1, A5, X6)
- [x] [D5](docs/prs/D5.md) ISP600 (wave 6; needs D1, A5, X6)
- [x] [D6](docs/prs/D6.md) Software ISP (wave 6; needs D1, A3)
- [x] [D7](docs/prs/D7.md) GPU ISP (wave 7; needs D6)
- [x] [D8](docs/prs/D8.md) Stills and DNG (wave 7; needs D6, B2)
- [x] [D9](docs/prs/D9.md) Sizes consumers ask for (wave 7; needs D2, D6)

## E. Encode and decode

Own encoders and Lemnos codec blocks first, FFmpeg for coverage.

- [x] [E1](docs/prs/E1.md) Codec trait and registry (wave 2; needs A1, A2)
- [x] [E2](docs/prs/E2.md) JPEG encoder (wave 3; needs E1, A3)
- [x] [E3](docs/prs/E3.md) H.264 encoder (wave 3; needs E1, A3)
- [x] [E4](docs/prs/E4.md) MJPEG and raw decode (wave 3; needs E1)
- [x] [E5](docs/prs/E5.md) Hardware codecs on Lemnos (wave 3; needs E1, A5)
- [x] [E6](docs/prs/E6.md) Scaling and conversion on Lemnos (wave 3; needs E1, A5)
- [x] [E7](docs/prs/E7.md) FFmpeg for coverage (wave 3; needs E1)

## F. Frames out

The camera service and every way a frame leaves it.

- [x] [F1](docs/prs/F1.md) The camera service (wave 5; needs B7, A4)
- [x] [F2](docs/prs/F2.md) The shared ring, without Orion (wave 2; needs A1, X5)
- [x] [F3](docs/prs/F3.md) Orion: the ring as a topic (wave 3; needs F2)
- [x] [F4](docs/prs/F4.md) Daedalus value (wave 3; needs F2, X7)
- [x] [F5](docs/prs/F5.md) Multi-camera grouping (wave 2; needs A1)
- [x] [F6](docs/prs/F6.md) C API (wave 6; needs F1, F2)
- [x] [F7](docs/prs/F7.md) GStreamer and PipeWire (wave 3; needs F2)

## G. Tools

Recording, replay, calibration, counters, preview, examples.

- [x] [G1](docs/prs/G1.md) Recording and replay (wave 4; needs A1, A6)
- [x] [G2](docs/prs/G2.md) Calibrate and tune (wave 7; needs C1, D6)
- [x] [G3](docs/prs/G3.md) Counters (wave 6; needs F1)
- [x] [G4](docs/prs/G4.md) Preview (wave 4; needs E2, F2)
- [x] [G5](docs/prs/G5.md) Examples (wave 6; needs F1, F2)

## H. Board runs and finish

Real hardware, then the lock and the docs.

Rulings of 2026-11-14 (the user): H2's camera capture (H2a) and H4 are dropped, H6 is kept.

- [ ] [H1](docs/prs/H1.md) Raze CM5: OV9782 native (wave 7; needs D2, F1, F3, C3, X2; board run). Closes with pass 3's P3-1..P3-5. Done: P3-1, P3-3, P3-5; LV's Raze run (R-PASS; proof 2 L.1: `isp=pisp`, 9000/9000, 0 gaps, one counted copy `pisp-be-input`); P3-2's next column (proof 2: isp0 11.67 ms median, raw 7.80 ms). **Blocked:** P3-2's and P3-4's dev column. dev reaches cameras only through V4L2 nodes, and raze-base has none (R-PASS 4: P2.d not run). It needs an image with the V4L2 nodes, or the user's ruling that the dev column is dropped.
- [ ] ~~[H2](docs/prs/H2.md) ZERO 3E: rkcif and ISP21, camera capture (H2a)~~ dropped 2026-11-14: no camera on the bench ZERO 3E.
- [ ] H2b ZERO 3E rk-mpp hardware encode fed synthetic frames. Off-board half landed (d8dddc10, `styx-block::rkvenc`). **Blocked:** Lemnos must state the six VEPU540 encodings in `rkvenc::MISSING` (asked through the coordinator); then a ZERO 3E board run.
- [ ] [H3](docs/prs/H3.md) K230D AI-CAM, built-in camera (wave 7; needs D4, F1; board run). Not started. **Blocked:** a K230D board slot (queued after M1/M2/DEBT).
- [ ] ~~[H4](docs/prs/H4.md) Cubie A7~~ dropped 2026-11-14: no camera or cable, none will be bought.
- [ ] [H5](docs/prs/H5.md) USB camera (wave 6; needs B4, B6, E4, F1; board run). **Blocked (hardware):** no USB webcam on either Raze.
- [ ] [H6](docs/prs/H6.md) libcamera camera on a Pi (wave 6; needs B5, F1; board run). **Blocked (hardware):** a stock Pi with a camera module, not on the bench.
- [ ] [H7](docs/prs/H7.md) Docs, lock, hand-over (wave 8; needs H1, H5, G5, F6, F7, G4). Docs half done (2bcf96b2), lock done. **Blocked:** the board-run table needs H1.

## S. The simulated camera: Styx's copies go as Lemnos's lands

Styx tests against simulated drivers of its own (crate `styx-sim`, and until X15 nine copies of it in test modules). They are not the hardware's owner's, and they differed from hardware where it mattered: identity equal to sequence hid the frame-id defect, and frames paced at a mode's nominal rate hide any rate error. Lemnos builds a simulated camera chain a consumer can reach (its brief: SIM1, real drivers over simulated hardware). Styx's copies are deleted as each part lands, not kept beside it and not all at the end.

- [x] S0 (now): X15, the nine test copies replaced by `styx-sim`, with a gate check that refuses a second definition. The pacing stays as it is, marked: no second copy of the frame-period arithmetic is written in Styx.
- [ ] S1, after Lemnos SIM1 series 1 is in Styx's lock (buses and dma-buf streams under `lemnosd --sim`, sequence counting dropped starts, ideal-start timestamps, the `sim.camera/1` control contract, the OV9782 register model with the real driver, the synthetic data-described sensor with small modes, the real receiver and back end over simulated hardware with the ordering scheduler and the per-job record, two or more chains per board, usable outside test trees and on aarch64): delete `styx-sim`'s simulated sensor, receiver and back end. Every test that used them runs on Lemnos's: the camera crates, the service, the C API, the measure tool, the examples' `--sim`, the emulated dry run of board binaries. Tests that assert order or no gaps use the virtual clock; end-to-end tests on the real clock assert invariants only.
  - **Gate not met (Lemnos's answer, 2026-11-23).** e199fcb9 has everything above except three rows Lemnos has queued after its board-path rows: SIM-PISP-INSTANCE (two or more PiSP chains per process), SIM-SYNTH-SENSOR (sensor modes given as data, small sizes) and SIM-CAMERA-CONTRACT (`sim.camera/1`). `lemnos-camera-sim` as a git dev-dependency at the locked Lemnos rev is the intended use.
  - Done in one pass once the three land (coordinator 2026-11-23). A partial move would delete nothing and leave two simulators side by side.
- [ ] S2, after series 2 (the front end over the real driver with statistics in the real layout that follow exposure x gain; embedded registers on the synthetic sensor; the codec block and the scaler, consumer-reachable): delete the simulated front end and ISP statistics, the simulated PiSP in `crates/isp/pisp/tests`, and the simulated codec block and scaler in `crates/codec/*/tests`. The control schedule is proven against embedded registers.
- [ ] S3, after series 3 (K230 and Rockchip ISP devices): delete the fakes those two ISP backends are tested against. `styx-sim` is removed from the workspace and the gate check with it.

## P. The HeliOS port's gaps (`docs/guide/dev-to-next.md`, Porting HeliOS)

Ordered by the coordinator; parked rows are recorded, not worked.

- [x] P1 (GAP 1) `styx-control-client`: the control socket's client, out of `styx-capi` (next 9ebbf67c).
- [x] P2 (GAP 5) lz4 MCAP chunks on import, `styx-record` feature `mcap-lz4` (next 593d5b18).
- [ ] P3 isp_second as a companion of the isp0 frame in `styx-daedalus`. Design accepted; **build when Eidos PYRAMID-ONCE reaches its hardware-level step** (the coordinator says when).
  - Pyramids are Eidos's (owner ruling 2026-11-21), so Styx builds no levels, scaler or pyramid type.
  - `StyxFrame::second()` gives the isp1 frame with the same id, held by the isp0 value for its Arc lifetime.
  - A `PairedFeed` pairs isp0 and isp1 by id.
  - The two are separate rings with their own pools and reserves, so the hold limit is not halved. A held value pins one frame on each, within each ring's share.
  - The feed keeps at most one unmatched isp1 frame.
  - (a) The service publishes output 1 before output 0 for each frame, so isp1 n is always there before isp0 n and the feed never holds back. This is free on PiSP. On the soft route it delays isp0 by output 1's time. The config doc must say: "isp_second on the soft route delays isp0 by output 1's time; it is for consumers that want a second stream, not for pyramids."
  - A missing companion is `None`, counted as `second_missing` (with `paired` and `unpaired`). The feed never waits.
  - A GPU node gets isp0 only: Daedalus moves the outer value, not the companion.
- [x] P5 CONTROLS-WAIT-NOT-POLL: measured in proof 2, item W (2026-11-23, Raze #2). Each watcher adds 0.1% of a core and 21 wakes/s to the service, against its idle 13.1% and 930 wakes/s; each watcher costs 0.14% itself. Negligible, so the 100 ms poll stays and no wait request is added. The original row: measure the service's idle CPU and wakes with 0, 1 and 2 `wait_controls_change` watchers on one camera (proof 2 sheet, item W).
  - If the cost is not negligible, add a wait the service answers: `wait <id> <seq> <timeout>`, or the seq on the frame socket. Not a different poll period.
- [ ] P6 (GAP 3, parked) A run-time request for N pyramid levels.
- [ ] P7 (GAP 4, parked) A replay source inside the service, serving a recording as a camera.
- [ ] P8 (GAP 6, parked) PNG replay.
- [x] P9a Sensor data in `styxrec` (format version 2): `Recorder::declare` / `sample`, `ClockPair` / `monotonic_from_boottime`, `Replay::next_item` (timestamp order, one frame of lookahead, late samples counted), v1 still read. Host cost in docs/guide/tools.md; the Raze figures are owed (kit /mnt/ssd-samsung/styx-targets/p9a-kit).
- [x] P9b `export_mcap` (lz4 by default; message indexes and summary), data channels as MCAP channels with encoding, schema and metadata; round trip through the `mcap` crate and `import_mcap` (tests/mcap_export.rs). Import keeps data topics as channels, moves `CLOCK_BOOTTIME` times by `/styx/clock`, counts what it drops in `ImportStats::losses`; zstd still refused.
- [x] P9c The `styx-record` command: `info` (header, channels with clocks and counts, frames, clean / not finished / truncated), `export` and `import` printing the counts and losses (`ExportStats::losses` new; data sequences count as lost only when not the channel's count). Static aarch64 musl build checked under qemu, byte-identical output to x86_64 (tests/cli.rs).
- [x] P9d `Replay::next_item` linear when its sample queue stays full (samples-only recordings, long gaps between frames): the queued byte count kept instead of summed per call, the queue a `VecDeque`, and the arena left with room for the queue again after a pack. A 57 MB recording of 210 000 samples: `styx-record info` 25.6 s -> 0.07 s (x86_64); tests/channels.rs `samples_only_replay_is_linear`.
- [ ] RECORD-WRITEBACK (facts first, no design) What the recorder does when a write blocks: `Recorder` has no queue and no drop policy; `record` blocks its thread for as long as the kernel holds the write, and frames are dropped upstream (a producer's pool, or `ReaderStats::skipped` / `ConsumerFault` on the camera socket). Host SSD: longest `record` 26-27 ms, drops in 2 of 8 runs.
  - **Raze #2 `/data`** (proof 2 item B, 2026-11-23, with the user's OK; eMMC BJTD4R, ext4, `dirty_ratio` 20):
    - one 20 s run wrote 2.17 GB;
    - **280 of 2400 frames dropped (11.7%)**;
    - **the longest `record` blocked 2195 ms**, with 3 records over a frame interval;
    - Dirty reached 504 MB.
  - The facts are complete. **Open:** what the recorder should do about it (no design yet; the user or coordinator decides whether one is wanted).

## Q. Proof 2's misses (Raze #2, 2026-11-23) and the remaining list's open rows

Results: styx-notes/3A-PROOF-2-RESULTS.md (raw outputs and pictures beside it). Image raze-base dev.1406-b94d9b84, kit from next efab3cf0. Written down for when work resumes; no fixes yet.

- [ ] Q1 (fix 4) A manual exposure lands one frame late. Set for frame F, the exposure is 4000 us from F+1 on; F keeps AE's value; nothing before F has it (proof 1: the same).
- [ ] Q2 (fix 5) The service's statistics share on PiSP is 3.39% of a core against the 2% bar. CPU per frame is 2.351 ms with statistics every frame and 1.504 ms at 1 in 4. Counting alone is 1.65% (R), and 2.07% with the unpack. **Break down the rest:** the unpack and the 3A loop. The soft route's 0.08% is not a valid reading, since that route is CPU-bound and the cost shows as fewer frames made.
- [ ] Q3 (fix 3) `awb_outside` stays `-` at 2435 K, below the tuning's warm end (2860 K), on both routes. The gains go the right way (934/2875).
- [ ] Q4 (A.1) `isp_made` reads 1376/1378: `isp_skipped=2`, the two start-up frames, constant. The expected count should take the skipped start frames into account.
- [ ] Q5 The receiver's `stop_us` is above `bound_us` (252-272 ms against 250 ms) in F.4 and F.6, with no log line and `stop idle`. Lemnos's driver; Styx reports it.
- [ ] Q6 `styx-measure read` leaves its service fields empty (`service_camera` null, `service_ns_per_frame`, requested, achieved, measured service), even with `--control`.
- [ ] Q7 The service refuses `fps_milli` above 240000, and refuses it missing. Mode 4's own 259.787 fps should be accepted, and a missing rate should mean the mode's own.
- [ ] Q8 (D, Lemnos's) The mode-4 frame (640x400 pBAA) has every odd row at black and no Bayer along a row. That image predates e199fcb9's Bayer fix, so the frame is re-checked on pin 15's image.
- [ ] Q9 (L-32/33) M33's reattach: the first frame after a reattach with no standby, and its time. **Blocked:** a Raze #2 slot (R-PASS 2 ran one lifetime, so the reattach half was not run).
- [ ] Q10 (L-34) The lend path's board proof needs a live FE path upstream of the back end. Code is done; **parked** (out of scope, coordinator 2026-11-21).
- [ ] Q11 (C-T) OV9782 colour tuning (black level, AWB, CCM) from a chart. **Blocked:** the user sets up the chart.
- [ ] Q12 A rate asked above the camera's ceiling allocates on the frame path. The service's steady test
  (`crates/out/service/tests/steady.rs`) with `fps_milli = 240000` against the simulated camera (it
  clamps to 120 fps) counted 953 allocations in 953 steady frames, one in every frame window; at
  60000 it counts 0 (2026-11-24). Found while shortening tests; held: speed is the only work now.
- [ ] Q13 `styx-measure` `readers::keep_and_stall_readers_against_the_share_of_two` failed once
  (`crates/tools/measure/tests/readers.rs:126`: "the reader keeping 2 runs on ... it held a frame
  past the keep window ... after 290 frames delivered; frames held at the cut: 2"): the pair's
  reader keeping 2 was cut, not the stalled one. All-features runs with the slow tests started
  first (nextest priority; that order was not kept): 2 failures in 8, this one and one whose
  output was not kept. In the usual order: 0 in 3 runs and the day's gates (2026-11-24, load
  89-114). Cause not shown: a reproduction must give that line
  and message. Since the wrong reader was cut, it may be a race in the service's choice of which
  reader to cut, not only an effect of test order. Held: speed is the only work now.
- [ ] Q14 `styx-service` `aaa` `the_3a_loop_on_the_simulated_camera` failed once
  (`crates/out/service/tests/aaa.rs:421`: "the manual exposure, to the line: 16444"): a manual
  exposure of 3000 us set for frame F (30 frames ahead) was not in effect at the first frame from F,
  which still had AE's 16444. 1 failure in 3 all-features runs at `--test-threads 78` (load
  214), 0 at 26 and 52 (2026-11-24). Starved (5 cores, `taskset -c 6-10`) at 52 threads it failed
  1 of 3 one assertion later (`aaa.rs:452`, "every frame from F has the manual gain": frame F
  had the old gain 8000, the 1500 began one frame after F); at 26 threads 0 of 5. Both are a
  frame-addressed control one frame late, as proof 2's fix 4 was on the board (+1 frame); not
  shown to be the same cause. The gate runs one test per core because of it. Cause not shown: a
  reproduction must give that line and message; it may be the set arriving after F under load
  (a test timing) or the frame-addressed control applied late (the service). Held: speed is the
  only work now. Also alone (2026-11-27, 20 runs of this test only, load 71-109, nice 10, not
  starved): the first line again ("the manual exposure, to the line: 16444", aaa.rs:458 since the
  Q15 diagnostics moved it) in 2 of 20 (logs/q15/run-15.log, run-20.log), about 1 s in.

- [ ] Q15 `styx-service` `aaa` `the_3a_loop_on_the_simulated_camera` failed once at
  `crates/out/service/tests/aaa.rs:528` ("the ISP thread allocates nothing per frame": left 6,
  right 0; "passes=60 isp_thread_allocations=6", wb_gains 1000/1000), 3.5 s in, in a gate at load
  75-109, nice 10, one test per core (2026-11-27, logs/b13-gate2.log; the change gated touched only
  the gate's scripts). A third line of the same test after Q14's two; first seen. Held: a product
  fault, and speed is the only work. Cause not shown: a reproduction must give that line and count.
  Since then the test prints, when this check fails, each ISP-thread allocation of the checked
  window with its size, the frame the test was waiting for and a backtrace (test-only).
  Cause named (2026-11-27, logs/q15/run-6.log: the test alone, 20 runs at load 71-109, nice 10;
  1 failure, the same check and count, 6): product code. A frame the ISP thread missed under load
  (`missed > 0 || !made` in the pace, `crates/out/service/src/isp.rs`) starts the "ISP short"
  status: `write!` into the pace's text grows it from empty (8, 16, 32, 64, 134 bytes, isp.rs:472),
  and `Slot::set_text` copies it with `to_string` (134 bytes, slot.rs:417, from isp.rs:840). All 6
  while waiting for frame 49. Held for the user with Q12-Q14: not fixed.

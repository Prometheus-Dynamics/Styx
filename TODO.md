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

- [ ] [H1](docs/prs/H1.md) Raze CM5: OV9782 native (wave 7; needs D2, F1, F3, C3, X2; board run)
- [ ] [H2](docs/prs/H2.md) ZERO 3E: rkcif and ISP21 (wave 7; needs D3, F1; board run)
- [ ] [H3](docs/prs/H3.md) K230D (wave 7; needs D4, F1; board run)
- [ ] [H4](docs/prs/H4.md) Cubie A7 (wave 7; needs E5, F1, D5; board run)
- [ ] [H5](docs/prs/H5.md) USB camera (wave 6; needs B4, B6, E4, F1; board run)
- [ ] [H6](docs/prs/H6.md) libcamera camera on a Pi (wave 6; needs B5, F1; board run)
- [ ] [H7](docs/prs/H7.md) Docs, lock, hand-over (wave 8; needs H1, H5, G5, F6, F7, G4)

## S. The simulated camera: Styx's copies go as Lemnos's lands

Styx tests against simulated drivers of its own (crate `styx-sim`, and until X15 nine copies of it in test modules). They are not the hardware's owner's, and they differed from hardware where it mattered: identity equal to sequence hid the frame-id defect, and frames paced at a mode's nominal rate hide any rate error. Lemnos builds a simulated camera chain a consumer can reach (its brief: SIM1, real drivers over simulated hardware). Styx's copies are deleted as each part lands, not kept beside it and not all at the end.

- [x] S0 (now): X15, the nine test copies replaced by `styx-sim`, with a gate check that refuses a second definition. The pacing stays as it is, marked: no second copy of the frame-period arithmetic is written in Styx.
- [ ] S1, after Lemnos SIM1 series 1 is in Styx's lock (buses and dma-buf streams under `lemnosd --sim`, sequence counting dropped starts, ideal-start timestamps, the `sim.camera/1` control contract, the OV9782 register model with the real driver, the synthetic data-described sensor with small modes, the real receiver and back end over simulated hardware with the ordering scheduler and the per-job record, two or more chains per board, usable outside test trees and on aarch64): delete `styx-sim`'s simulated sensor, receiver and back end. Every test that used them runs on Lemnos's: the camera crates, the service, the C API, the measure tool, the examples' `--sim`, the emulated dry run of board binaries. Tests that assert order or no gaps use the virtual clock; end-to-end tests on the real clock assert invariants only.
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
- [ ] P5 CONTROLS-WAIT-NOT-POLL: measure the service's idle CPU and wakes with 0, 1 and 2 `wait_controls_change` watchers on one camera (proof 2 sheet, item W).
  - If the cost is not negligible, add a wait the service answers: `wait <id> <seq> <timeout>`, or the seq on the frame socket. Not a different poll period.
- [ ] P6 (GAP 3, parked) A run-time request for N pyramid levels.
- [ ] P7 (GAP 4, parked) A replay source inside the service, serving a recording as a camera.
- [ ] P8 (GAP 6, parked) PNG replay.
- [x] P9a Sensor data in `styxrec` (format version 2): `Recorder::declare` / `sample`, `ClockPair` / `monotonic_from_boottime`, `Replay::next_item` (timestamp order, one frame of lookahead, late samples counted), v1 still read. Host cost in docs/guide/tools.md; the Raze figures are owed (kit /mnt/ssd-samsung/styx-targets/p9a-kit).
- [ ] P9b MCAP export of `styxrec` (lz4), data channels as MCAP channels with their encoding and schema, round-trip through the mcap crate and `import_mcap`; import keeps HeliOS .mcap data topics as channels (losses reported), BOOTTIME moved by dev's /styx/clock records.
- [ ] RECORD-WRITEBACK (facts first, no design) What the recorder does when a write blocks: `Recorder` has no queue and no drop policy; `record` blocks its thread for as long as the kernel holds the write, and frames are dropped upstream (a producer's pool, or `ReaderStats::skipped` / `ConsumerFault` on the camera socket). Host SSD: longest `record` 26-27 ms, drops in 2 of 8 runs. Raze `/data` run is item B of the proof 2 sheet (needs the user's OK: it writes outside /run).

# Styx `next`: the PR list

Every PR has a brief in `docs/prs/`. `python3 scripts/pr-status.py` shows what is ready.
Needs named `X<n>` are outside blocks, listed in `docs/design.md`.

## A. Foundation

The frame type, formats, kernels, the Lemnos client and one simulated camera. Everything else builds on these.

- [ ] [A1](docs/prs/A1.md) FrameLease, slimmed (wave 1; needs nothing)
- [ ] [A2](docs/prs/A2.md) Formats (wave 1; needs nothing)
- [ ] [A3](docs/prs/A3.md) SIMD kernels (wave 2; needs A1, A2)
- [ ] [A4](docs/prs/A4.md) Controls and metadata (wave 2; needs A1)
- [ ] [A5](docs/prs/A5.md) Lemnos client for cameras (wave 2; needs A1, X1)
- [ ] [A6](docs/prs/A6.md) One simulated camera (wave 3; needs A5)
- [ ] [A7](docs/prs/A7.md) no_std check (wave 2; needs A1, A2)

## B. Cameras

Every way a camera comes up: native on Lemnos, libcamera, V4L2, and the choice between them.

- [ ] [B1](docs/prs/B1.md) Native: find and assemble a camera (wave 3; needs A5)
- [ ] [B2](docs/prs/B2.md) Native: modes and start order (wave 4; needs B1)
- [ ] [B3](docs/prs/B3.md) Native: controls on frame identities (wave 5; needs B2, A4)
- [ ] [B4](docs/prs/B4.md) Native USB cameras (wave 3; needs A5)
- [ ] [B5](docs/prs/B5.md) libcamera backend (wave 3; needs A1, A4)
- [ ] [B6](docs/prs/B6.md) V4L2 backend (wave 3; needs A1, A4)
- [ ] [B7](docs/prs/B7.md) Choosing a backend (wave 4; needs B1, B5, B6)
- [ ] [B8](docs/prs/B8.md) Hotplug (wave 5; needs B7)

## C. 3A and tuning

Making the image good on the native path. Tuning ships with Styx.

- [ ] [C1](docs/prs/C1.md) 3A algorithms (wave 3; needs A4)
- [ ] [C2](docs/prs/C2.md) One controller (wave 4; needs C1)
- [ ] [C3](docs/prs/C3.md) Tuning files (wave 4; needs C1)
- [ ] [C4](docs/prs/C4.md) Autofocus (wave 5; needs C2, A5)

## D. ISPs

Turning 3A results into ISP jobs, and the software and GPU ISPs for chips without one.

- [ ] [D1](docs/prs/D1.md) Neutral ISP settings (wave 5; needs C2)
- [ ] [D2](docs/prs/D2.md) PiSP (wave 6; needs D1, A5)
- [ ] [D3](docs/prs/D3.md) PiSP slices (wave 7; needs D2)
- [ ] [D4](docs/prs/D4.md) rkisp (wave 6; needs D1, A5)
- [ ] [D5](docs/prs/D5.md) K230 ISP (wave 6; needs D1, A5)
- [ ] [D6](docs/prs/D6.md) Software ISP (wave 6; needs D1, A3)
- [ ] [D7](docs/prs/D7.md) GPU ISP (wave 7; needs D6)
- [ ] [D8](docs/prs/D8.md) Stills and DNG (wave 7; needs D6, B2)
- [ ] [D9](docs/prs/D9.md) Sizes consumers ask for (wave 7; needs D2, D6)

## E. Encode and decode

Own encoders and Lemnos codec blocks first, FFmpeg for coverage.

- [ ] [E1](docs/prs/E1.md) Codec trait and registry (wave 2; needs A1, A2)
- [ ] [E2](docs/prs/E2.md) JPEG encoder (wave 3; needs E1, A3)
- [ ] [E3](docs/prs/E3.md) H.264 encoder (wave 3; needs E1, A3)
- [ ] [E4](docs/prs/E4.md) MJPEG and raw decode (wave 3; needs E1)
- [ ] [E5](docs/prs/E5.md) Hardware codecs on Lemnos (wave 3; needs E1, A5)
- [ ] [E6](docs/prs/E6.md) Scaling and conversion on Lemnos (wave 3; needs E1, A5)
- [ ] [E7](docs/prs/E7.md) FFmpeg for coverage (wave 3; needs E1)

## F. Frames out

The camera service and every way a frame leaves it.

- [ ] [F1](docs/prs/F1.md) The camera service (wave 5; needs B7, A4)
- [ ] [F2](docs/prs/F2.md) Styx socket (wave 2; needs A1)
- [ ] [F3](docs/prs/F3.md) Orion topic (wave 2; needs A1)
- [ ] [F4](docs/prs/F4.md) Daedalus value (wave 2; needs A1)
- [ ] [F5](docs/prs/F5.md) Frame age (wave 3; needs F2)
- [ ] [F6](docs/prs/F6.md) Multi-camera grouping (wave 2; needs A1)
- [ ] [F7](docs/prs/F7.md) C API (wave 6; needs F1, F2)
- [ ] [F8](docs/prs/F8.md) GStreamer and PipeWire (wave 3; needs F2)

## G. Tools

Recording, replay, calibration, counters, preview, examples.

- [ ] [G1](docs/prs/G1.md) Recording and replay (wave 4; needs A1, A6)
- [ ] [G2](docs/prs/G2.md) Calibrate and tune (wave 7; needs C1, D6)
- [ ] [G3](docs/prs/G3.md) Counters (wave 6; needs F1)
- [ ] [G4](docs/prs/G4.md) Preview (wave 4; needs E2, F2)
- [ ] [G5](docs/prs/G5.md) Examples (wave 6; needs F1, F2)

## H. Board runs and finish

Real hardware, then the lock and the docs.

- [ ] [H1](docs/prs/H1.md) Raze CM5: OV9782 native (wave 8; needs D3, F1, F3, C3, X2; board run)
- [ ] [H2](docs/prs/H2.md) ZERO 3E: rkcif and ISP21 (wave 7; needs D4, F1; board run)
- [ ] [H3](docs/prs/H3.md) K230D (wave 7; needs D5, F1; board run)
- [ ] [H4](docs/prs/H4.md) Cubie A7 (wave 6; needs E5, F1; board run)
- [ ] [H5](docs/prs/H5.md) USB camera (wave 6; needs B4, B6, E4, F1; board run)
- [ ] [H6](docs/prs/H6.md) libcamera camera on a Pi (wave 6; needs B5, F1; board run)
- [ ] [H7](docs/prs/H7.md) Docs, lock, hand-over (wave 9; needs H1, H5, G5, F7, F8, G4)

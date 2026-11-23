# Board runs

What has run on real hardware, with the figures recorded. Everything not listed here is tested on
the host only (see the table in [README.md](README.md)).

## R-PASS: the camera service on a Raspberry Pi CM5 carrier (2026-11-22)

The board image runs Lemnos `bbb3c420` (lemnosd, its kernel module and the board file), with no
other camera user. The service ran as root, one camera: an OV9782 on the CSI receiver, then the
PiSP front and back end. Each run was 9000 frames.

**Modes:**
- Mode 0: 1280x800, 10-bit packed Bayer. Asked 120.00, it runs at 119.967 fps (whole lines),
  and every run measured 119.966.
- Mode 4: 640x400, 10-bit. Not used for figures: Lemnos's sensor table gives empty odd rows
  there, and the fix is Lemnos's.

**Styx:** `next` `d3638daa`, then `9fce7b49` (stop order) for the last two passes.

### Latency and cost per path

| Stream | What makes it | Frames | Capture to reader, min / median / p99 / max | Service CPU per frame | Copies per frame |
|---|---|---|---|---|---|
| raw | the camera | 9000 of 9000, 0 gaps | 7.66 / 7.80 / 8.32 / 10.95 ms | 0.78 ms (PiSP running beside) | 0 |
| `isp0`, `isp = "auto"` | the PiSP back end | 9000 of 9000, 0 gaps | 10.12 / 10.35 / 11.93 / 16.05 ms | 0.78 ms | 1 at `pisp-be-input` (reads W×H×10/8, writes W×H×2) |
| `isp0`, `isp = "soft"` | the soft ISP in the service | 9000 of 9000, 0 gaps | 11.04 / 11.93 / 13.24 / 15.86 ms | 4.18 ms (one core) | 0 |
| raw, `styx-measure --soft-isp NV12` | the soft ISP in the reader | 9000 of 9000, 0 gaps | 7.80 ms median to the reader; 4.26 ms ISP median | (reader: 4.32 ms) | 0 |

Service memory: 2.9 MB resident with PiSP, 8.5 MB with the soft ISP.

### Rules checked

| Rule | Result |
|---|---|
| The processed stream comes up at 9 buffers: `buffers=9 reserve=7 isp=pisp`, form `raw10-unpack-to-bayer16`, no refusal | pass |
| PiSP output 0 at the asked format and size, `isp_refused=0` | pass, over 54,277 frames |
| A raw reader holding 1 older frame beside an `isp0` reader (`--keep 1`): no cut, the service's Lemnos reader never at its hold | pass (`reserve_cuts=0`, `lemnos_at_limit=0`) |
| A raw reader holding 2 older frames (`--keep 2`) | cut, as the rule says |
| `lemnos-ctl list` row for the sensor while streaming | `running ok`, no reason |
| Stop releases the receiver's memory: no kernel `DMA did not stop` line, 0 dma-bufs left, and the camera starts again | pass with the receiver stopped first (3 of 3). It failed with the sensor stopped first (2 of 2: buffers pinned until a reboot, restart refused). The order is now receiver first (`9fce7b49`). |
| Start to first frame after a restart | about 0.18 s |

### Not run there

- The back end's output buffer count: Lemnos's tool does not show it.
- `dev` beside it: `dev` reaches cameras only through V4L2 nodes, and this image gives the CSI
  receiver and the back end to Lemnos's module, so there are none.
- Per-stage CPU counters: the profiler was not on hand.
- White balance and exposure control: the service ran fixed exposure and gain with no 3A
  (`isp_3a=none`), so the pictures carry the lamp's colour.

## H1: the raw path, earlier passes (2026-11-13 and 2026-11-14)

The raw path through Lemnos on the same board, before the processed stream: up to 239.9 fps in
the binned mode with 0 gaps (transport only: that mode's picture has the empty rows above), the
reader hold limits, the soft ISP's cost per stage, and a non-root service and reader. The figures
are in the H1 reports.

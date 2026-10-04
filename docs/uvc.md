# USB cameras from userspace (`styx-uvc`)

Styx normally reaches USB cameras through the kernel's `uvcvideo` driver and its own V4L2
backend. `styx-uvc` is the alternative: it speaks the USB Video Class (UVC 1.0–1.5) itself,
over Linux usbfs (`/dev/bus/usb/BBB/DDD`), with nothing but `libc` — no `uvcvideo`, no
libusb. In Styx it is the opt-in backend `BackendKind::Uvc` (cargo feature `uvc`, off by
default).

## Why

* **Works where `uvcvideo` is missing or old.** Minimal or locked-down kernels, containers
  that only pass `/dev/bus/usb`, kernels whose `uvcvideo` lacks a quirk a camera needs: usbfs
  is enough. Fixes and quirks ship with the application, not with the kernel.
* **Exact timestamps.** Every frame carries the camera's own capture time (PTS) and clock
  references (SCR). `styx-uvc` maps them to `CLOCK_MONOTONIC` and stamps each frame with when
  the sensor captured it. Measured on a C270 at 30 fps: frame-to-frame jitter 0.000 ms (sd)
  against 1.9 ms through `uvcvideo`, whose timestamps are the completion times of 4 ms
  transfers (they come out 32 or 36 ms apart; `hwtimestamps=1` changed nothing here).
* **Per-frame metadata.** PTS, SCR, when the first and last payload crossed the bus, the
  frame's damage (lost packets, error bit, short or overflowing frame, FID-only end): in
  `FrameMeta::uvc()` (`UvcFrameMeta`), next to the V4L2-style sequence number.
* **Control over buffering.** URBs in flight, packets per URB (latency vs wake-ups), frame
  buffers kept, whether damaged frames are dropped or delivered flagged.
* **Async-first.** The stream is the usbfs descriptor: `UvcStream::next().await` on any
  executor (styx-graph's reactor), `next_blocking`, or `try_next` from your own poll loop. No
  thread of its own.

## How it works

```
sysfs (/sys/bus/usb/devices)   enumerate: identity, speed, cached descriptors, which driver
                               holds each interface (no device access needed)
descriptors                    VideoControl: camera terminal, processing unit, extension
                               units, dwClockFrequency; VideoStreaming: formats (YUY2, NV12,
                               MJPEG, frame-based H.264, ...), frame sizes, intervals, colour,
                               alternate settings and their bandwidth
usbfs (styx-kernel::usbfs)     claim the video interfaces (DISCONNECT_CLAIM to detach a
                               kernel driver from just those, when allowed), control
                               transfers, SET_INTERFACE, a ring of URBs (SUBMIT / REAP /
                               DISCARD) whose memory the ring owns until the kernel gave
                               every URB back
PROBE / COMMIT                 format, frame and interval → dwMaxPayloadTransferSize;
                               the smallest alternate setting that carries it
payloads → frames              header parse (FID, EOF, PTS, SCR, STI, ERR), assembly into
                               pooled buffers (one copy, as uvcvideo), handed out as is
clock                          bus time of every packet; SCR → device clock line; PTS → host
hotplug                        kernel uevents (netlink, lemnos_linux::uevent) → sysfs rescan → added / removed /
                               drivers changed
```

Frame assembly follows `uvcvideo`'s rules, written down in `payload.rs`: a frame ends at EOF
or when FID toggles; the stream is joined at the first frame boundary (a partial first frame
is never delivered); after EOF the next frame must toggle FID; a lost packet, a failed one,
the ERR bit, a short or overflowing uncompressed frame damage it. Damaged frames are dropped
by default (counted; the sequence number shows the gap) or delivered flagged.

**Timestamps.** Isochronous URBs submitted ahead occupy consecutive (micro)frames, so the
n-th packet ends exactly `T0 + (n+1)·125 µs` into the stream; the earliest any URB was reaped
(sliding one-second minimum) gives `T0` without the scheduling jitter. SCR samples (device
clock at a USB frame start, placed on the bus by the first packet carrying that frame number)
fit a line from device clock to host time; the frame's PTS through it is the capture time.
Without PTS/SCR (or on bulk endpoints) the timestamp is the first payload's arrival.

**Controls** are the camera terminal's and processing unit's, under the V4L2 ids, units and
menus `uvcvideo` gives them (`V4L2_CID_BRIGHTNESS`, `V4L2_CID_EXPOSURE_ABSOLUTE` in 100 µs,
`V4L2_CID_EXPOSURE_AUTO` 0..3, `V4L2_CID_POWER_LINE_FREQUENCY`, white balance, gain, focus,
zoom, ...), so code written against a UVC camera through V4L2 works unchanged. Typed helpers:
`set_exposure(Duration)`, `set_auto_exposure`, `set_white_balance(kelvin)`,
`set_power_line_frequency`.

## Using it

```rust
// styx-uvc on its own.
let (cams, _errors) = styx_uvc::enumerate();
let dev = styx_uvc::UvcDevice::open(cams[0].clone(), styx_uvc::OpenOptions::default())?;
let cfg = dev.find_mode(*b"MJPG", 1280, 720, Some(333_333)).unwrap(); // 100 ns units
let mut stream = dev.start(cfg)?;
let frame = stream.next().await?; // or next_blocking(timeout)
println!("{} bytes captured at {:?} (from PTS: {})", frame.data.len(), frame.timestamp,
         frame.timestamp_from_pts);
```

Through Styx (`--features uvc`): `probe_all` lists every UVC camera, also those `uvcvideo`
holds. A camera both can see is one `ProbedDevice` (matched on the USB bus path) with the V4L2
backend first: **`uvcvideo` stays the default**, and the planner considers the userspace
backend only when asked for it (`Frames::...backend(BackendKind::Uvc)`,
`CaptureRequest::backend(BackendKind::Uvc)`) or when nothing else has the camera.

Using it on a camera `uvcvideo` holds needs the interfaces free: either unbind it
(`echo 3-1:1.0 > /sys/bus/usb/drivers/uvcvideo/unbind`), or let Styx detach it for the
capture (`StyxConfig::uvc_detach_kernel_driver(true)` or `STYX_UVC_DETACH=1`). Only the video
function's interfaces are detached (a microphone on the same device keeps `snd-usb-audio`);
`uvcvideo` is bound again when the capture ends, and its `/dev/video*` nodes come back.
Without either, opening fails with "interface 0 is bound to the kernel driver uvcvideo".
Access to `/dev/bus/usb` needs root or a udev rule (`MODE="0660", GROUP="plugdev"`).

`UvcConfig` (`StyxConfig::backends.uvc`): `detach_kernel_driver`, `urbs` (5),
`packets_per_urb` (32 = 4 ms at high speed; fewer lowers latency, adds wake-ups),
`deliver_damaged`. Supervised captures (`CaptureStartPolicy::resilient()`) reconnect after an
unplug like V4L2 ones.

Examples: `crates/uvc/examples/uvc_raw.rs` (the crate alone: list, stream with per-frame
PTS/SCR, async, hotplug, replug) and `examples/04_performance/uvc_capture.rs` (through Styx,
userspace against `uvcvideo`).

## Measured

CM5 (Cortex-A76, kernel 6.12.47), Logitech C270 (046d:0825, UVC 1.00, high speed,
isochronous) on 2026-10-03, `uvc_capture capture <backend> <format> 30 600` with
`UVC_FIXED_RATE=1` (exposure may not lower the rate), 600 frames after 30 to settle; the
V4L2 runs with `uvcvideo` bound, the userspace runs with it unbound, minutes apart, same
dim room. CPU is the scheduler's runtime (`se.sum_exec_runtime`), not tick samples.

| | `uvcvideo` + Styx V4L2, YUYV 640x480 | userspace, YUYV 640x480 | `uvcvideo` + V4L2, MJPEG 1280x720 | userspace, MJPEG 1280x720 |
|---|---|---|---|---|
| `start()` returns | 52.9 ms | 30.8 ms (137.7 ms detaching `uvcvideo`) | 52.4 ms | 29.1 ms |
| open → first frame (the camera's own start) | 491 ms | 491 ms | 1106 ms | 1097 ms |
| rate | 29.49 fps | 29.69 fps | 29.79 fps | 29.79 fps |
| frames dropped (short frames, the C270's) | 6 | 2 | 0 | 0 |
| timestamp interval sd; p50 / p99 | 3.77 ms; 32.02 / 36.01 ms | 1.94 ms (the two drops); 33.570 / 33.571 ms | 1.95 ms; 32.01 / 36.00 ms | **0.000 ms**; 33.570 / 33.570 ms |
| timestamp | first URB's completion | capture time (PTS) | first URB's completion | capture time (PTS) |
| last payload → frame in the application | — | 2.0 ms mean, 3.9 ms p99 | — | 2.0 ms mean, 3.9 ms p99 |
| first payload → frame in the application | 33.5 ms (from the first URB's completion) | 35.4 ms (from the payload's bus time) | 33.5 ms | 35.4 ms |
| CPU, process | 0.07% of a core | 1.51% | 0.06% | 1.44% |
| CPU, all tasks (kernel workers included) | 1.85% | 2.39% | 1.25% | 2.28% |

* Latency is the same: `uvcvideo` timestamps a frame when the URB holding its first packet
  completes (0–4 ms after the packet), so its "first payload → application" is 2 ms shorter on
  average for the same delivery. Both deliver a frame when the URB holding its last packet
  completes: 0–4 ms after the last packet with 32-packet URBs (`packets_per_urb` lowers it).
* CPU: the userspace path costs ~0.5–1% of a core more for 18 MB/s (usbfs allocates, zeroes and
  copies each URB's buffer at reap; `uvcvideo` copies once in a kernel worker). Mapping the
  URB buffers from usbfs (`StreamConfig::mapped_urbs`) avoids the kernel copy, but DMA is not
  cache-coherent on the CM5, the mapping is uncached, and reading it cost more (and damaged
  4 of 300 frames); off by default.
* An early version polled with sub-millisecond timeouts truncated to 0 and spun (9% of a
  core); `UsbDevice::wait` now rounds up.

Also run on the CM5: controls through both backends (brightness 128→200, manual exposure
10 ms then 100 ms, gain, power line frequency; same values read back, same mean
luma 25.3 → 79–80 → 81.3); unplug/replug by `authorized` 0/1 under a supervised capture
(V4L2: first frame 815 ms after re-authorizing; userspace with detach, since `uvcvideo` binds
again on re-enumeration: 942 ms; without detach the restart is refused with the
kernel-driver error, as designed); hotplug events (removed 50 ms after `authorized=0`; added, and
`uvcvideo` bound, 0.4 s after `authorized=1`); async `next().await` (300 frames, 0 dropped); `uvcvideo` back on the camera
with its `/dev/video*` nodes after every run.

## Limitations

* Tested on one camera (C270: UVC 1.00, isochronous, YUYV and MJPEG). Bulk endpoints,
  UVC 1.5 cameras, SuperSpeed and frame-based (H.264) formats are covered by unit tests on
  synthetic descriptors and payloads only. A bulk payload larger than one URB (12 MiB of URBs
  in total) is refused.
* Bulk streams have no PTS mapping (no bus clock to place SCRs): their timestamp is the arrival.
* No still-image capture (method 2/3), no extension-unit controls (raw access only through
  `UvcDevice::usb()`), no pan/tilt, no status interrupt endpoint (asynchronous control changes,
  the button), no UVC metadata node equivalent beyond `UvcFrameMeta`.
* Recordings keep the frame's V4L2-style metadata (sequence, size, error flag), not PTS/SCR.
* Frames are host memory (one copy from the URB buffers, as `uvcvideo`); they cross processes
  by copy to memfd, not as dma-bufs.
* Controls are read at probe only while no driver holds the camera; with `uvcvideo` bound they
  are listed when the capture starts.

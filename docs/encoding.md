# Encoding

Styx encodes through FFmpeg (feature `codec-ffmpeg`, loaded at runtime): software encoders
(libx264, libx265, libopenh264, MJPEG), V4L2 memory-to-memory hardware encoders
(`*_v4l2m2m`) and VA-API (`h264_vaapi`, `hevc_vaapi` on Intel and AMD). Hardware encoders are
registered when their device exists, and are checked by encoding a probe frame the first time
they are looked up.

```rust
use styx::prelude::*;
use styx_codec::ffmpeg::{FfmpegEncoderOptions, FfmpegH264Encoder};

let encoder = FfmpegH264Encoder::with_options_for_input(
    FourCc::NV12,
    FfmpegEncoderOptions {
        bitrate: 4_000_000,
        // Encoder-specific FFmpeg options, e.g. a faster preset for the smallest devices.
        codec_options: &[("preset", "ultrafast")],
        ..Default::default()
    },
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Low latency by default

`FfmpegEncoderOptions::low_latency` (on by default) makes encoders emit a packet for every frame
as it is encoded. For libx264 and libx265 it sets `tune=zerolatency` and the `superfast` preset
(`LOW_LATENCY_PRESET`) unless `codec_options` names them. Other encoders already work frame by
frame.

libx264's own defaults are built for offline encoding. On a Raspberry Pi CM5 at 1280x720 NV12,
one thread:

| Settings | First packet after | Encode | Memory (anonymous RSS) |
|---|---|---|---|
| libx264 defaults (before) | 41 frames | 61 ms (below 30 fps) | 140 MB |
| `zerolatency`, `veryfast` | 0 frames | 24 ms | 20 MB |
| `zerolatency`, `superfast` (default) | 0 frames | 14.5 ms | 20 MB |
| `zerolatency`, `ultrafast` | 0 frames | 9 ms | 13 MB |

`ultrafast` fits the most cameras per core at some cost in compression; `veryfast` compresses
better. Set `low_latency: false` to get libx264's defaults back, e.g. for offline transcoding.

## Frames reach the encoder without copies

When the encoder takes the frame's pixel format and size unchanged, Styx hands FFmpeg the frame's
own memory instead of copying it into an encoder-owned frame:

- **Camera buffers** (V4L2 mmap, libcamera dma-buf) and aligned host frames are read in place.
  FFmpeg holds a reference to the buffer until it is done with it, so the camera buffer returns
  to the driver only then.
- **VA-API:** NV12 dma-bufs, such as libcamera and V4L2 buffers, are imported as the GPU surface
  itself, with no copy at all. Other frames are uploaded to a surface straight from their own
  memory. Imports need a layout the GPU accepts (AMD wants 256-byte row pitches); frames that do
  not qualify are uploaded. On an AMD RX 6800 XT at 1280x720 this takes the CPU per frame from
  0.67 ms to 0.25 ms, and the output is byte-identical to uploading.
- Frames that need a format or size conversion (for example NV12 into libopenh264, which only
  takes YUV420P) go through swscale into a staging frame as before.

Planes must start on 32-byte boundaries with 32-byte-aligned strides to be read in place, which
camera buffers and Styx's aligned pools satisfy.

## Not yet

- Encoders that accept DRM-PRIME frames directly (ffmpeg-rockchip's `*_rkmpp`, the Raspberry Pi
  FFmpeg fork's `*_v4l2m2m`) could take camera dma-bufs without the VA-API step; they are not
  wired up or validated on hardware yet.
- The Raspberry Pi 5 / CM5 has no hardware H.264 encoder; use libx264 as above.

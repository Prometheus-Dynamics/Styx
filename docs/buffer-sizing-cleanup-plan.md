# Buffer Sizing Cleanup Plan

Goal: remove arbitrary buffer sizes where Styx can derive a safer value from the frame format, resolution, packet type, observed payload size, or an explicit runtime limit.

## Tasks

- [x] Finish and commit the shared compressed encoder output pool sizing change.
- [x] Verify the shared encoder pool report shows compressed packet-sized chunks instead of raw input frame-sized chunks.
- [x] Audit MJPEG netcam capture buffers that are currently sized from raw RGB pixels while storing compressed JPEG packets.
- [x] Change MJPEG netcam compressed packet buffers to use a bounded compressed-packet estimate or an explicit max-packet limit.
- [x] Add focused tests for MJPEG netcam buffer sizing so compressed packet storage cannot regress to raw RGB-sized chunks.
- [x] Inspect codec-owned `BufferPool::lazy(1 << 20, 4)` defaults in FFmpeg, MJPEG, JPEG, TurboJPEG, and image-any paths.
- [x] Confirm whether codec packet output uses pooled storage or bypasses it through owned packet replacement.
- [x] Replace codec-owned arbitrary `1 MiB` compressed-output pool sizing where it actually retains memory.
- [x] Keep raw decode/output pools sized from decoded format and resolution instead of compressed input size.
- [ ] Review fixed shared pool retain counts such as min `2` and spare `4` and decide whether they should remain defaults or become runtime policy.
- [ ] Add memory-report fields or debug output needed to distinguish shared pool capacity, active leases, retained spares, and codec-owned scratch buffers.
- [x] Run targeted unit tests for frame sizing, runtime memory reports, codec pool sizing, and MJPEG netcam sizing.
- [x] Run the runtime memory probe with libcamera, graph-pipeline, hooks, and codec-ffmpeg features enabled.
- [ ] Record before/after memory observations in the runtime debugging docs.

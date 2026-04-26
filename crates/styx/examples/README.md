# Examples

Canonical examples for the intended facade:

- `capture_virtual`: smallest blocking capture request example.
- `low_latency_preview`: latest-frame preview path with queue depth tuned for freshness.
- `reliable_recording`: record every frame to disk with queue and pool sizing biased toward completeness.
- `latest_frame_fanout`: split one source into multiple latest-only consumers without adding backpressure.
- `file_replay`: replay recorded files through the same capture facade.
- `netcam_capture`: ingest MJPEG over HTTP with explicit timeout and backoff tuning.

Specialized examples remain for feature-specific surfaces such as:

- `async_pipeline`
- `probe_and_select`
- `v4l2_hardware_bench`
- `pipeline_health`
- `ffmpeg_scale`
- `libcamera_ffmpeg_preview`

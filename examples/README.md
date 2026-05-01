# Examples

Examples are organized as the top-level `styx-examples` package:

- `00_quickstart`: first-run capture and pipeline examples
- `01_capture`: capture sources, async capture, file/netcam/simulation inputs
- `02_graph`: Daedalus graph integration and `FrameLease` graph flow
- `03_codecs`: decode/encode examples
- `04_performance`: benchmark and zero-copy validation examples
- `05_apps`: preview/recording application-style examples

Run examples through the `styx-examples` package with explicit features:

```bash
cargo run -p styx-examples --bin quickstart_capture_virtual
cargo run -p styx-examples --bin quickstart_runtime_memory_report
cargo run -p styx-examples --features camera-graph --bin camera_graph_metrics
```

Canonical examples for the intended facade:

- `capture_virtual`: smallest blocking capture request example.
- `runtime_memory_report`: process-level and pipeline-attached memory telemetry report.
- `low_latency_preview`: latest-frame preview path with queue depth tuned for freshness.
- `reliable_recording`: record every frame to disk with queue and pool sizing biased toward completeness.
- `latest_frame_fanout`: split one source into multiple latest-only consumers without adding backpressure.
- `file_replay`: replay recorded files through the same capture facade.
- `netcam_capture`: ingest MJPEG over HTTP with explicit timeout and backoff tuning.

Specialized examples remain for feature-specific surfaces such as:

- `async_pipeline`
- `probe_and_select`
- `graph_fanout`
- `v4l2_hardware_bench`
- `pipeline_health`
- `ffmpeg_scale`
- `libcamera_ffmpeg_preview`

The default CI surface builds the portable subset with `async`, `file-backend`, and `netcam`. Hardware, preview window, FFmpeg, libcamera, V4L2, and simulation examples stay behind their matching feature flags.

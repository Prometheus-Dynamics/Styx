# Test fixtures

- `c270_720p_rst.mjpeg`: 8 consecutive 1280x720 MJPEG frames from a Logitech C270
  (`046d:0825`) over V4L2, concatenated. 4:2:2, restart markers on every frame, and the small
  stream defects ("extraneous bytes before marker") this camera emits. Used by the MJPEG luma
  benchmarks and perf smoke.

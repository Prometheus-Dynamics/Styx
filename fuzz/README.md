# Fuzzing

Coverage-guided fuzz targets for the parsers that read untrusted bytes. The same inputs are also
covered by deterministic corruption tests that run with `cargo test`
(`crates/styx/src/replay/corruption_tests.rs`, `crates/codec/src/corruption_tests.rs`); these
targets are for longer runs.

| Target | Input |
|---|---|
| `replay_reader` | MCAP and `.styxrec` recordings (`open_recording_reader`) |
| `mjpeg_decode` | MJPEG frames through the turbojpeg, turbojpeg-luma and zune decoders |
| `ipc_messages` | camera service and frame client messages (`styx::ipc`); decoded requests must encode back to themselves |

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run replay_reader -- -max_total_time=600 -rss_limit_mb=512
cargo +nightly fuzz run mjpeg_decode -- -max_total_time=600 -rss_limit_mb=512
cargo +nightly fuzz run ipc_messages -- -max_total_time=600 -rss_limit_mb=512
```

`-rss_limit_mb` makes libFuzzer fail on inputs that make a parser allocate far more than the
input, which matters on small devices. Seed corpora: any `.mcap`/`.styxrec` recording, and the
frames of `testing/fixtures/c270_720p_rst.mjpeg`.

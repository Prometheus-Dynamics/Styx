# Styx v2 API Cleanup Tasks

This tracks the approved cleanup work for removing example-shaped APIs from the
core Rust library before the v2 release. The goal is to keep `styx` focused on
capture, graph, codec, policy, sink, and config primitives while examples compose
those primitives into preview, recording, analysis, netcam, virtual, and demo
workflows.

## Tasks

- [x] Remove example workflow presets from `StyxConfig`.
  - Remove `StyxConfig::low_latency_preview()`.
  - Remove `StyxConfig::reliable_recording()`.
  - Remove `StyxConfig::netcam_preview()`.
  - Keep the direct builder-style usage in examples:
    ```rust
    StyxConfig::new()
        .capture_queue_depth(1)
        .capture_pool(2, 1 << 18, 2)
    ```
  - Move any remaining workflow naming into local example helpers.

- [x] Split the monolithic `StyxConfig` into focused config structs.
  - Introduce the intended public config shape:
    ```rust
    pub struct StyxConfig {
        pub capture: CaptureConfig,
        pub transforms: TransformConfig,
        pub backends: BackendConfig,
    }

    pub struct BackendConfig {
        pub v4l2: V4l2Config,
        pub libcamera: LibcameraConfig,
        pub netcam: NetcamConfig,
        pub file: FileBackendConfig,
    }
    ```
  - Move backend-specific fields out of capture-global tunables.
  - Keep existing builder ergonomics where possible through forwarding methods
    or obvious nested builder methods.

- [x] Rename graph policies around behavior instead of application workflows.
  - Replace `preview_policy()` with `latest_only()`.
  - Replace `recording_policy(capacity)` with `bounded_blocking(capacity)`.
  - Replace `analysis_policy(capacity, max_lag_frames)` with
    `bounded_drop_oldest(capacity, max_lag_frames)`.
  - Update examples to assign workflow names locally:
    ```rust
    let preview: GraphPolicy = latest_only();
    let recording = bounded_blocking(64);
    let analysis = bounded_drop_oldest(8, 3);
    ```

- [x] Replace workflow-specific sink helpers with generic sink registration.
  - Remove or stop exporting `register_preview_sink_node`.
  - Remove or stop exporting `register_analysis_sink_node`.
  - Remove or stop exporting `register_recorder_sink_node` if the generic sink
    API can cover it cleanly.
  - Prefer the generic shape:
    ```rust
    graph.register_frame_sink_node(
        SinkNodeConfig::new("preview")
            .policy(GraphPolicy::latest_only()),
        sink,
    );
    ```
  - Keep workflow helper functions inside examples if they materially reduce
    repeated example code.

- [x] Keep the preview window behind an explicit feature and out of the main
  prelude.
  - Use the approved option C shape:
    ```rust
    #[cfg(feature = "preview-window")]
    use styx::extras::preview_window::PreviewWindow;
    ```
  - Avoid exporting example UI code through the normal core API path.
  - Keep feature-gated docs clear that this is a helper, not the core preview
    abstraction.

- [x] Move preview and recording terminology out of codec selection.
  - Replace codec fields such as `preview_format` and `recording_codec` with
    capability-based fields:
    ```rust
    EncoderFamilySpec {
        input_formats,
        output_formats,
        latency,
        hardware,
        streamable,
    }
    ```
  - Replace preview/recording selector helpers with capability selectors:
    ```rust
    CodecSelector::new()
        .media_type(MediaType::Video)
        .latency(CodecLatency::Low)
        .streamable(true)
    ```
  - Let applications decide which selected codec is used for preview or
    recording.

- [x] Replace virtual capture demo helpers with `CaptureRequest` source builders.
  - Remove or stop exporting `make_virtual_rgb_device`.
  - Remove or stop exporting `open_virtual_rgb`.
  - Keep the approved request-based shape:
    ```rust
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .format(PixelFormat::Rgb24)
            .resolution(width, height)
    )
    ```
  - Keep any one-call RGB helpers local to examples or test support.

- [x] Move all capture sources toward the same request-based source builder
  pattern.
  - Added `CaptureSource`, `VirtualSourceConfig`, `VirtualCaptureConfig`,
    `NetcamSourceConfig`, and `FileSourceConfig`.
  - Virtual and netcam examples now use `CaptureRequest::virtual_source(...)`
    and `CaptureRequest::netcam_source(...)`.
  - File replay examples now use `CaptureRequest::file_source(...)`.
  - Prefer source constructors through `CaptureRequest`, for example:
    ```rust
    CaptureRequest::netcam_source(
        NetcamSourceConfig::new(url)
            .resolution(width, height)
            .fps(fps)
    )
    ```
  - Apply the same pattern to V4L2, libcamera, file replay, virtual, and any
    other first-class capture source where it makes sense.
  - Keep direct backend types available when they are useful as lower-level
    building blocks.

- [ ] Move simulation capture out of core.
  - Treat simulation as example-only.
  - Move simulation backend code, assets, and workflow helpers into the
    `examples` crate.
  - Remove simulation from the core prelude and core capture API.
  - Keep only minimal test fixtures in core if unit tests need synthetic frames.

- [x] Replace `MediaPipelineBuilder::record_output` with generic sink wiring.
  - Remove the recording-specific builder shortcut from the generic pipeline
    builder.
  - Use the approved generic sink shape:
    ```rust
    MediaPipelineBuilder::new()
        .raw_frames()
        .sink("recording", FrameRecorder::new(...))
    ```
  - Keep `FrameRecorder` if recording remains a first-class library feature.

- [x] Add the final intended public config/API vocabulary.
  - `StyxConfig`
  - `CaptureConfig`
  - `BackendConfig`
  - `V4l2Config`
  - `LibcameraConfig`
  - `NetcamConfig`
  - `FileBackendConfig`
  - `FileSourceConfig`
  - `TransformConfig`
  - `GraphPolicy`
  - `SinkPolicy`
  - `CodecSelector`
  - `VirtualCaptureConfig`

- [x] Update examples after the core API cleanup.
  - Move workflow-specific helper functions into `examples`.
  - Keep examples short by composing the new primitives locally.
  - Avoid adding public library APIs solely to reduce example line count.

- [x] Update documentation after the core API cleanup.
  - Update README snippets.
  - Update crate docs.
  - Update changelog with the final removed/changed API list.
  - Make sure no release risk notes are added to the v2 changelog entry.

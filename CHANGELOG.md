# Changelog

All notable changes to this workspace should be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning.

## [Unreleased]

### Added

- Added generic frame layout metadata in `styx-core-rs` for CV and media consumers, including
  storage kind, channel order, bit depth, chroma subsampling, packed pixel schema, plane schema,
  and `FourCc::layout_info()`.
- Added host frame validation and access helpers on `FrameLease`, including layout validation,
  host-readable/writable checks, export/materialization capability checks, and frame alias
  detection.
- Added visible-row frame access APIs for stride-aware CV processing, including immutable and
  mutable row views, all-plane visible views, contiguous visible-plane fast paths, visible payload
  byte accounting, tight-packing detection, and explicit visible-plane copy helpers.
- Added host-owned frame allocation helpers for known layouts, same-layout allocation with custom
  timestamps, allocation with stride and plane-size alignment controls, and layout-preserving
  output allocation.
- Added plane shape metadata and multi-plane validation for packed, Bayer, NV12/NV21, and
  I420/YU12/YV12 frame layouts, including overlap detection for shared-address-space backings.
- Added export capability reporting on external frame backings so memfd/dmabuf-style backings can
  advertise zero-copy export support without treating every external backing as exportable.

## [2.0.0] - 2026-05-01

### Added

- Added a release-oriented multi-crate workspace versioned as `2.0.0` across `styx`,
  `styx-core-rs`, `styx-capture`, `styx-codec`, `styx-libcamera`, `styx-v4l2`, and
  `styx-examples`.
- Added runtime capture configuration through `StyxConfig`, including queue depth, buffer pool
  sizing, capture enqueue timeouts, V4L2 worker timing, libcamera startup/control/idle timing,
  netcam HTTP/backoff timing, file image cache limits, and transform pool sizing.
- Added focused public config/source types for the v2 API surface, including `CaptureConfig`,
  `BackendConfig`, `V4l2Config`, `LibcameraConfig`, `NetcamConfig`, `FileBackendConfig`,
  `TransformConfig`, `VirtualSourceConfig`, `VirtualCaptureConfig`, `NetcamSourceConfig`,
  `FileSourceConfig`, `GraphPolicy`, and `SinkPolicy`.
- Added typed capture startup policy support with resilient retries, transient libcamera retry
  handling, optional control dropping on rejected controls, and optional TDN fallback behavior.
- Added async capture helpers for Tokio users, including async receive/control methods,
  async startup retry sleeps, async netcam workers, and blocking-worker helpers for CPU-heavy
  pipeline stages.
- Added netcam MJPEG and optional FFmpeg-backed video ingestion with configurable request,
  connect, read, retry, stop-poll, and queue-send behavior.
- Added file replay support for image and optional video sources, including playback controls
  and decoded image cache tuning.
- Added simulation capture support behind `simulation-bevy`, including runtime state,
  visualization/readback plumbing, and depth/RGB output handling.
- Added graph-pipeline support through the Daedalus plugin integration, including frame nodes,
  source/sink nodes, codec nodes, runtime nodes, control event routing, fanout policies, and
  graph telemetry reporting.
- Added richer observability with `tracing` spans/events, stage metrics, health reports, queue
  telemetry, drop reasons, residency transitions, external backing tracking, memory statistics,
  and recent stage/control error reporting.
- Added Linux zero-copy oriented frame residency and backing support, including shared memfd
  pools, export/import helpers, residency capability reporting, and shared decode/encode paths
  where codecs support them.
- Added typed codec policy and selector improvements, including `CodecImplementationId`,
  typed preferred lookup/process APIs, codec implementation priority, hardware bias controls,
  codec family selectors, and registry sizing configuration.
- Added raw decoder coverage for common packed, planar, semi-planar, Bayer, and mono formats,
  including NEON-accelerated paths where available and feature-gated raw decoder registration.
- Added runtime-configurable transform and dynamic image staging pools, with pool telemetry for
  memory and allocation debugging.
- Added pipeline memory telemetry for Linux shared decode and encode pools so high-resolution
  zero-copy pool sizing and retained capacity are visible at runtime.
- Added capture retry and shutdown telemetry for startup retries, netcam reconnects, async drop
  detaches, worker joins, drains, and teardown latency.
- Added typed service event kind accessors for service, pipeline worker, sink lifecycle, and
  recording lifecycle events so consumers can filter event streams without matching strings.
- Added hotplug/watch runtime support with sync and async subscription paths.
- Added release validation assets and scripts, including feature-combination checks, perf smoke
  checks, file-size checks, Docker facade tests, and organized example binaries.

### Changed

- Bumped the public workspace release from `1.0.0` to `2.0.0` and updated crate README install
  snippets to match.
- Reworked the public capture API around `CaptureRequest`, `CaptureHandle`, `CaptureSource`,
  typed modes/descriptors/controls, and request-local runtime configuration.
- Changed virtual, netcam, and file replay examples to use request-based source builders:
  `CaptureRequest::virtual_source`, `CaptureRequest::netcam_source`, and
  `CaptureRequest::file_source`.
- Changed simulation support to live under the explicit `styx::simulation` feature module instead
  of the core capture API surface; simulation examples now import simulation APIs directly.
- Split capture, codec, core buffer/format/queue, V4L2 probing, and libcamera probing concerns
  into dedicated crates while keeping `styx` as the high-level facade crate.
- Reorganized examples into top-level workflow groups for quickstart, capture, graph, codecs,
  performance, and app-style examples.
- Tightened feature gating so heavy stacks remain opt-in: FFmpeg, netcam, file backend,
  libcamera, V4L2, preview windows, Bevy simulation, hotplug, graph pipeline, serde, and schema
  support are all controlled through explicit features.
- Changed pipeline processing to preserve stage error details through result-returning methods
  while keeping infallible iterator-style convenience methods for simple callers.
- Changed async netcam MJPEG enqueue behavior to use async queue waiting with a timeout instead
  of blocking Tokio runtime workers on synchronous queue sends.
- Changed bounded queue close/send synchronization so queue closure has a clearer ordering
  against concurrent send attempts.
- Changed pipeline teardown so stopping one pipeline no longer resets process-wide transform pool
  configuration that may be used by another live pipeline.
- Changed capture backend pool sizing to use a typed `PoolLimits` value instead of passing raw
  `(min, bytes, spare)` tuples through backend code.
- Changed codec registry matching to normalize implementation IDs consistently and added typed
  APIs for callers that want to avoid stringly typed implementation preferences.
- Changed runtime codec selector boundaries to compare normalized `CodecImplementationId` values
  instead of raw implementation strings.
- Changed raw decoder implementations to share descriptor construction, owned output allocation,
  and Linux shared-output boilerplate.
- Changed codec residency detection to use `FourCc` helpers and descriptor methods instead of
  repeated ad hoc compressed-format checks.
- Changed observability to record more runtime data, including async queue waits/wakes,
  per-stage p50/p95 timing, drop causes, graph drops/latest replacements, sink lifecycle events,
  and recorder indexing events.
- Changed netcam MJPEG parsing so sync and async workers share header, content-length, boundary,
  and oversized-frame parser state while keeping their IO paths separate.
- Changed libcamera manager lifecycle handling to use typed runtime configuration, probe cache
  TTL controls, active camera use guards, and optional idle-stop behavior.
- Changed graph policy helpers from workflow names to behavior names: `latest_only`,
  `bounded_blocking`, and `bounded_drop_oldest`.
- Changed pipeline recording wiring from `record_output(recorder)` to the generic
  `.sink("recording", recorder)` builder shape.
- Changed codec family metadata and helpers to describe output/capability behavior instead of
  preview/recording application roles.

### Removed

- Removed the old crate-local `crates/styx/examples` layout in favor of the workspace-level
  `examples` crate and grouped example directories.
- Removed unconditional process-wide cleanup of transform and image staging pools from pipeline
  teardown; callers can still explicitly reset configurable global pools when they intentionally
  want to clear them.
- Removed several ad hoc magic tuples and backend-local pool sizing conventions in favor of typed
  configuration and centralized tunables.
- Removed repeated string matching for codec implementation preferences where typed
  `CodecImplementationId` APIs can now be used.
- Removed workflow-specific config presets from the core API: `StyxConfig::low_latency_preview`,
  `StyxConfig::reliable_recording`, and `StyxConfig::netcam_preview`.
- Removed workflow-specific preview/analysis/recorder graph sink helpers from public exports in
  favor of generic frame sink registration.
- Removed virtual/netcam/file demo constructors from the main prelude; use the request-based
  source builders for application code or import lower-level constructors from `capture_api` when
  needed.
- Removed simulation assets from the facade crate and moved them under the examples crate.
- Removed legacy compatibility assumptions around backend startup and feature availability:
  disabled optional backends now report typed `BackendMissing` errors instead of being hidden
  behind implicit fallback behavior.

### Fixed

- Fixed async netcam backpressure so full output queues no longer block Tokio core runtime
  workers during MJPEG frame enqueue.
- Fixed a close/send race in the bounded queue by synchronizing close with the same wait-state
  lock used by send paths.
- Fixed cross-pipeline transform pool interference caused by resetting process-wide pool state
  during unrelated pipeline teardown.
- Fixed release-readiness issues around pool sizing readability and typed codec preference
  ergonomics without breaking existing string-based APIs.
- Fixed async drop behavior so dropping a capture handle from a Tokio runtime does not block on
  worker joins.
- Fixed blocking pipeline worker failures so terminal decode/encode/graph errors are returned and
  emitted through service lifecycle events.
- Fixed netcam reconnect behavior so ended or failing MJPEG streams back off instead of reconnecting
  in a tight loop.

## [1.0.0] - 2026-04-19

- Standardized the workspace layout, docs, CI, linting, and helper scripts.
- Centralized workspace dependencies and brought all crate members under the shared root configuration.
- Added Docker-backed facade validation for the virtual-camera flow.
- Added `scripts/check-file-sizes.sh`, `scripts/ci.sh`, and `scripts/repo-clean.sh`.

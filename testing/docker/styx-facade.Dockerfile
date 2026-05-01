FROM rust:1.94.0

WORKDIR /workspace

COPY . .

RUN cargo build -p styx-examples --no-default-features --features "async,file-backend,preview-window" \
    --bin quickstart_capture_virtual \
    --bin low_latency_preview \
    --bin async_pipeline \
    --bin reliable_recording

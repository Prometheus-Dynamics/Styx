FROM rust:1.94.0

WORKDIR /workspace

COPY . .

RUN cargo build -p styx --features "async,file-backend" \
    --example capture_virtual \
    --example low_latency_preview \
    --example async_pipeline \
    --example reliable_recording

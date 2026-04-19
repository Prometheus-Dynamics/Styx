FROM rust:1.94.0

WORKDIR /workspace

COPY . .

RUN cargo build -p styx --features "async,file-backend" \
    --example capture_virtual \
    --example capture_and_decode \
    --example async_pipeline \
    --example record_and_replay

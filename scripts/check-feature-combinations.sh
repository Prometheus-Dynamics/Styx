#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

declare -a checks=(
    "default::cargo check -p styx"
    "no-default::cargo check -p styx --no-default-features"
    "async::cargo check -p styx --no-default-features --features async"
    "netcam::cargo check -p styx --no-default-features --features netcam"
    "file-backend::cargo check -p styx --no-default-features --features file-backend"
    "v4l2::cargo check -p styx --no-default-features --features v4l2"
    "libcamera::cargo check -p styx --no-default-features --features libcamera"
    "raw-decoders::cargo check -p styx --no-default-features --features raw-decoders"
    "codec-jpeg-decoder::cargo check -p styx --no-default-features --features codec-jpeg-decoder"
    "codec-ffmpeg::cargo check -p styx --no-default-features --features codec-ffmpeg"
    "codec-mozjpeg::cargo check -p styx --no-default-features --features codec-mozjpeg"
    "codec-turbojpeg::cargo check -p styx --no-default-features --features codec-turbojpeg"
    "graph-pipeline::cargo check -p styx --no-default-features --features graph-pipeline"
    "netcam-video::cargo check -p styx --no-default-features --features netcam,netcam-video"
    "file-backend-video::cargo check -p styx --no-default-features --features file-backend,file-backend-video"
    "simulation-bevy::cargo check -p styx --no-default-features --features simulation-bevy"
    "async-netcam::cargo check -p styx --no-default-features --features async,netcam"
    "async-netcam-file::cargo check -p styx --no-default-features --features async,netcam,file-backend"
    "serde::cargo check -p styx --no-default-features --features serde"
    "schema::cargo check -p styx --no-default-features --features schema"
    "release-linux-media::cargo check -p styx --no-default-features --features async,netcam,file-backend,codec-jpeg-decoder,raw-decoders,graph-pipeline,v4l2,libcamera"
    "all-features::cargo check -p styx --all-features"
)

for check in "${checks[@]}"; do
    name="${check%%::*}"
    command="${check#*::}"
    echo "==> Checking styx feature set: $name"
    read -r -a args <<<"$command"
    "${args[@]}"
done

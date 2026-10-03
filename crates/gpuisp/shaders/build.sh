#!/bin/sh
# Compile the GPU ISP's compute shaders to the SPIR-V the crate embeds (`src/shaders.rs`).
# Needs `glslc` (shaderc); the outputs are committed, so building the crate does not.
set -eu
cd "$(dirname "$0")"
for s in full half stats; do
    glslc --target-env=vulkan1.2 -O -o "$s.spv" "$s.comp"
done

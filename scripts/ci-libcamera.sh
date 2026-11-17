#!/usr/bin/env bash
# Builds libcamera from source into a prefix, for CI: the hosted runner's packaged libcamera is
# older than the versions styx-libcamera is checked against, and a feature CI does not build is
# not covered. The prefix is cached by the workflow; a prefix that already holds this version is
# left alone.
#   ci-libcamera.sh <prefix>
set -euo pipefail
prefix="$1"
version="${LIBCAMERA_VERSION:-v0.6.0}"
if PKG_CONFIG_PATH="$prefix/lib/pkgconfig" pkg-config --exists libcamera 2> /dev/null; then
  echo "libcamera $(PKG_CONFIG_PATH="$prefix/lib/pkgconfig" pkg-config --modversion libcamera) is in $prefix"
  exit 0
fi
src="$(mktemp -d)"
git clone --depth 1 --branch "$version" https://git.libcamera.org/libcamera/libcamera.git "$src"
# Only what a build and a camera-less test need: the library, the UVC and virtual-media
# pipelines, no applications, bindings, documentation or tests.
meson setup "$src/build" "$src" \
  --prefix "$prefix" --libdir lib --buildtype release \
  -Dpipelines=uvcvideo,vimc -Dipas=vimc \
  -Dgstreamer=disabled -Dv4l2=disabled -Dcam=disabled -Dqcam=disabled \
  -Dlc-compliance=disabled -Dtest=false -Ddocumentation=disabled -Dpycamera=disabled \
  -Dtracing=disabled -Dudev=disabled
ninja -C "$src/build" install
rm -rf "$src"

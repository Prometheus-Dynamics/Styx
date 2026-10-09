# Opt-in build speed-ups for a developer's shell (docs/development.md "Faster local builds").
# Source it, from any checkout or worktree (bash or zsh):
#
#   . scripts/dev-env.sh
#
# What it sets, each only when asked for or when the tool is installed:
#
#   RUSTC_WRAPPER=sccache    when sccache is on PATH (STYX_SCCACHE=0: not). sccache keeps
#                            compiled dependencies across checkouts, worktrees and `cargo clean`,
#                            so a new worktree compiles only Styx's own crates. It does not
#                            change what cargo builds (no rebuild when switched on or off).
#   CARGO_BUILD_BUILD_DIR    when STYX_BUILD_ROOT names a directory: cargo's intermediate files
#                            (everything but the final binaries and libraries, which stay in
#                            target/) go to STYX_BUILD_ROOT/<hash of the checkout's path>, one
#                            directory per checkout, so worktrees never wait for each other's
#                            lock. For a target/ on a slow or busy disk: put STYX_BUILD_ROOT on
#                            an SSD or a tmpfs. Changing it is a full rebuild for that checkout.
#
# Nothing here is needed: without it cargo behaves as usual. It sets no RUSTFLAGS and no
# linker: either one, switched on in one shell and not in another, rebuilds every crate each
# time (docs/development.md explains the linker choice). Sourcing it again re-reads the
# variables; `STYX_SCCACHE=0 . scripts/dev-env.sh` turns sccache off in this shell.

if [ "${STYX_SCCACHE:-1}" != 0 ] && command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=sccache
    echo "dev-env: RUSTC_WRAPPER=sccache (cache: $(sccache --show-stats 2>/dev/null |
        sed -n 's/^Cache location *//p' | head -1))"
elif [ "${RUSTC_WRAPPER:-}" = sccache ]; then
    unset RUSTC_WRAPPER
    echo "dev-env: sccache off"
fi

if [ -n "${STYX_BUILD_ROOT:-}" ]; then
    mkdir -p "$STYX_BUILD_ROOT"
    export CARGO_BUILD_BUILD_DIR="$STYX_BUILD_ROOT/{workspace-path-hash}"
    echo "dev-env: CARGO_BUILD_BUILD_DIR=$CARGO_BUILD_BUILD_DIR"
fi

# Development

Styx follows the shared Prometheus Dynamics workspace layout:

- `crates/`: published and internal Rust crates
- `docs/`: repository-level guidance
- `testing/`: validation notes and CI-facing test surfaces
- `.github/workflows/`: GitHub Actions pipelines

## Validation Surface

Use these commands for the default local validation loop:

```bash
./scripts/repo-clean.sh
cargo fmt --all -- --check
./scripts/check-file-sizes.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
```

## Examples

User-facing examples live under `crates/styx/examples`. Prefer exercising new end-to-end behavior there before adding heavier CI-specific fixtures.

See [`testing.md`](testing.md) for the default and example-focused validation surfaces.

## Tooling

- Rust toolchain is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml)
- Root dependency versions are aligned in [`Cargo.toml`](../Cargo.toml)
- Local validation entrypoint lives in [`scripts/ci.sh`](../scripts/ci.sh)
- Local cleanup entrypoint lives in [`scripts/repo-clean.sh`](../scripts/repo-clean.sh)
- CI entrypoints live in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)

## Dependency Policy

- Library-facing error types use `thiserror`.
- Shared runtime instrumentation should use `tracing` instead of introducing parallel logging stacks.
- Backend or codec alternatives stay behind stable feature names and should only expand when they represent a real media/runtime tradeoff.

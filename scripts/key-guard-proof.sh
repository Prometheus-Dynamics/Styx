#!/usr/bin/env bash
# The proof that a script left out of the reuse key cannot give a stale green (scripts/gate.py
# test_readable, scripts-hidden). It plants a test in styx-math that reads scripts/pr-status.py by
# a path built at run time (no string literal names the file, so the file is in no key), runs
# scripts/check.sh, and passes only when that run is red on the planted test with a permission
# error. The planted test is taken out again whatever happens.
#
#   scripts/key-guard-proof.sh          (in a worktree with no changes to crates/core/math)
#
# 2026-11-24: red on the planted test, `Os { code: 13, kind: PermissionDenied }`, every other
# lane green, 45 s.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
file=crates/core/math/src/tests.rs
git diff --quiet -- "$file" || { echo "key-guard-proof: $file has changes" >&2; exit 1; }
log=$(mktemp)
trap 'git checkout -q -- "$file"; rm -f "$log"' EXIT
cat >> "$file" << 'EOF'

// Planted by scripts/key-guard-proof.sh, never committed: a script read by a path built at run time.
#[test]
fn planted_reads_a_script_by_a_joined_path() {
    let name = ["pr-", "status", ".py"].concat();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("../../../scripts").join(name);
    std::fs::read(&path).expect("read the planted script");
}
EOF
status=0
scripts/check.sh styx-math > "$log" 2>&1 || status=$?
if [ "$status" != 0 ] && grep -q 'FAIL .*styx-math tests::planted_reads_a_script_by_a_joined_path' "$log" \
  && grep -q 'read the planted script: Os { code: 13, kind: PermissionDenied' "$log"; then
  echo "key-guard-proof: held: the planted read was refused and the run is red"
  exit 0
fi
echo "key-guard-proof: FAILED: the run was not red on the planted read (exit $status):" >&2
grep -E 'planted|GATE|PermissionDenied' "$log" >&2 || tail -20 "$log" >&2
exit 1

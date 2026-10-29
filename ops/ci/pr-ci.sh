#!/usr/bin/env bash
# Canonical local PR gate for jeryu-intelligence. host-ci prefers this script and posts the
# `jeryu-intelligence/required` check-run from its exit status; .github/workflows/ci.yml runs
# the same lanes on the GitHub mirror so the two surfaces cannot diverge.
set -euo pipefail

# The forge's gate runs this script with no argument, and then the proof lane
# always runs. `--without-proof-lane` is for the GitHub mirror's workflow only:
# its checkout cannot reach the forge's protected main, the one baseline the
# lane may judge against, so it skips that lane, says so loudly, and runs
# everything else. Any other argument is refused.
run_proof_lane=1
for arg in "$@"; do
  case "$arg" in
    --without-proof-lane) run_proof_lane=0 ;;
    *) echo "[pr-ci] unknown argument: $arg" >&2; exit 2 ;;
  esac
done

# BEGIN GENERATED JANKURAI PIN — DO NOT EDIT
# The governed Jankurai identity is the binary installed on this host and its
# installation receipt: require_jankurai verifies both and exports JERYU_JANKURAI_*
# from the receipt. The one pin of record is jeryu-tool's tool-manifest.toml.
# END GENERATED JANKURAI PIN


repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# jeryu governs the worker count from live load; never default high.
if [ -n "${JERYU_CI_JOBS:-}" ]; then
  JOBS="${JERYU_CI_JOBS}"
elif command -v jeryu-ci-governor >/dev/null 2>&1; then
  JOBS="$(jeryu-ci-governor 2>/dev/null || echo 8)"
else
  JOBS=8
fi
export JERYU_CI_JOBS="$JOBS"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$JOBS}"

# Resolve and verify the absolute governed auditor before any lane runs.
source ops/ci/lib.sh
require_jankurai

# Verify only this PR worktree against the family manifest; the post-rollout
# control-plane check verifies the complete canonical family. The renderer is
# jeryu-tool's, outside this checkout: a missing one fails closed, and one inside
# this checkout (which the pull request under test could have written) is refused.
if [ -z "${JERYU_TOOL_RENDER+x}" ]; then
JERYU_TOOL_RENDER="$repo_root/../jeryu-tool/ops/render-tool-manifest.sh"
fi
[ -n "$JERYU_TOOL_RENDER" ] || { echo "[pr-ci] missing JERYU_TOOL_RENDER" >&2; exit 1; }
[ -x "$JERYU_TOOL_RENDER" ] || { echo "[pr-ci] renderer is not executable: $JERYU_TOOL_RENDER" >&2; exit 1; }
render_real="$(realpath "$JERYU_TOOL_RENDER")"
case "$render_real/" in
"$(realpath "$repo_root")/"*) echo "[pr-ci] refusing a renderer inside this checkout: $JERYU_TOOL_RENDER" >&2; exit 1 ;;
esac
echo "[pr-ci] jankurai pin drift check" >&2
consumer_repo="$(awk -F'\"' '/^workspace =/ {print $2; exit}' agent/audit-policy.toml)"
bash "$render_real" --check --repo "$consumer_repo" \
--repo-root "$consumer_repo=$repo_root"

# Ordinary compilation -- cargo check, the symbol-schema example, the workspace nextest run, the
# codegraph tests, the tool-build `cargo run`, and the sibling jeryu-api tests when that checkout is
# present -- builds in the gate runner's retained build cache when the gate host offers one
# (JERYU_CI_BUILD_CACHE_DIR; jeryu-ci-runner ops/pr-gate/README.md, "Reusing ordinary compilation
# without retaining proof"). Nothing this gate runs measures a build directory: there is no coverage
# or profile build and no lane reads a binary out of target/, and every report it writes
# (.jankurai/, target/jankurai/, target/security/, target/artifact-support/) goes to a literal path
# outside the cache. Unset (a laptop, another host), cargo builds into target/ as before.
# BEGIN BUILD CACHE GUARD
if [ -n "${JERYU_CI_BUILD_CACHE_DIR:-}" ]; then
  # Accept only this checkout's own physical target/build-cache, owned by this user (the layout the
  # gate runner gives), never a symlink or a path elsewhere; and never reuse incremental state.
  cache_dir="$JERYU_CI_BUILD_CACHE_DIR"
  cache_top="$repo_root"
  if [[ -L "$cache_top/target" || -L "$cache_dir" || ! -d "$cache_dir" || ! -O "$cache_dir" \
        || "$(cd "$cache_dir" && pwd -P)" != "$(cd "$cache_top" && pwd -P)/target/build-cache" ]]; then
    echo "refusing JERYU_CI_BUILD_CACHE_DIR=$cache_dir: only this checkout's physical target/build-cache is accepted" >&2
    exit 1
  fi
  CARGO_TARGET_DIR="$(cd "$cache_dir" && pwd -P)"
  export CARGO_TARGET_DIR CARGO_INCREMENTAL=0
  echo "[pr-ci] ordinary compilation in ${CARGO_TARGET_DIR} (the gate runner's build cache)" >&2
fi
# END BUILD CACHE GUARD
echo "[pr-ci] (jobs=$JOBS) standard lanes" >&2
bash ops/ci/fast.sh
JERYU_SPLIT_FULL_CHECK=1 bash ops/ci/check.sh
bash ops/ci/score.sh
bash ops/ci/security.sh
bash ops/ci/artifact_support.sh

# Evidence must pass before the required check succeeds.
if [[ "$run_proof_lane" -eq 1 ]]; then
  bash ops/ci/proof_evidence.sh
else
  echo "[pr-ci] PROOF LANE SKIPPED (--without-proof-lane): this run is not jeryu-intelligence/required. The forge's jeryu-intelligence/required runs ops/ci/proof_evidence.sh against its protected main on every head." >&2
fi

echo "[pr-ci] workspace test suite" >&2
cargo nextest run --workspace --build-jobs "$JOBS" --test-threads "$JOBS"
echo "[pr-ci] codegraph oracle + tool-build lanes" >&2
bash ops/ci/codegraph-oracle.sh
bash ops/ci/codegraph-tool-build.sh
echo "[pr-ci] jeryu-intelligence OK" >&2

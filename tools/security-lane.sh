#!/usr/bin/env bash
# Canonical supply-chain entrypoint for the proof-evidence lane
# (`jankurai security run . --strict`), and for local runs.
#
# Strict always: ops/ci/security.sh skips gitleaks and actionlint when they are
# not installed, which is right for a laptop but would let the lane record a
# green, strict security result for a run that scanned nothing. Here a missing
# scanner fails instead. `--strict` is accepted for the auditor's call; any
# other argument is refused rather than ignored.
set -euo pipefail

for arg in "$@"; do
  case "$arg" in
    --strict) ;;
    *) printf 'security lane: unknown argument %s\n' "$arg" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
command -v gitleaks >/dev/null 2>&1 || {
  printf 'security lane: gitleaks is not installed; the secret scan cannot run\n' >&2
  exit 1
}
if [[ -d .github/workflows ]]; then
  command -v actionlint >/dev/null 2>&1 || {
    printf 'security lane: actionlint is not installed; the workflow lint cannot run\n' >&2
    exit 1
  }
fi
if [[ "${JERYU_SECURITY_NETWORK:-0}" == "1" && -f deny.toml ]]; then
  command -v cargo-deny >/dev/null 2>&1 || {
    printf 'security lane: JERYU_SECURITY_NETWORK=1 but cargo-deny is not installed\n' >&2
    exit 1
  }
fi
exec bash ops/ci/security.sh

#!/usr/bin/env bash
# Canonical supply-chain entrypoint used by local and protected CI.
# Delegate to this repository's security checks and propagate their exit status.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
exec bash ops/ci/security.sh "$@"

#!/usr/bin/env bash
# Shared local CI defaults for this split repo. Keep this file source-only.
set -euo pipefail

JERYU_CI_JOBS="${JERYU_CI_JOBS:-8}"
export JERYU_CI_JOBS

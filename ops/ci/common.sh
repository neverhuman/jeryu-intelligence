#!/usr/bin/env bash
# Shared local CI defaults for this split repo. Keep this file source-only.
set -euo pipefail

# BEGIN GENERATED JANKURAI PIN — DO NOT EDIT
# The governed Jankurai identity is the binary installed on this host and its
# installation receipt: require_jankurai verifies both and exports JERYU_JANKURAI_*
# from the receipt. The one pin of record is jeryu-tool's tool-manifest.toml.
# END GENERATED JANKURAI PIN


JERYU_CI_JOBS="${JERYU_CI_JOBS:-8}"
export JERYU_CI_JOBS

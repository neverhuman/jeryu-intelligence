#!/usr/bin/env bash
set -euo pipefail
source ops/ci/lib.sh
cargo check -p jeryu-codegraph
cargo nextest run -p jeryu-codegraph --lib

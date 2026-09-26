# Testing

Use the local CI entrypoints before pushing changes:

- `just fast`
- `just check`
- `just score`
- `just security`
- `just artifact-support`

`scripts/ci-local.sh` delegates to the same `ops/ci/*.sh` lanes used by the
GitHub workflow. `scripts/ci-doctor.sh` checks the required local tools.

`just check` also runs the drift test of the generated symbol-row contract.
Regenerate it from this repository root with
`cargo run --locked -p jeryu-codegraph --example symbol-schema > contracts/codegraph.schema.json`.
The Rust serialization uses `crate_name`, accepts empty strings and unknown input
fields, and bounds `line` to an unsigned 32-bit integer. The schema preserves
those semantics.

Agent-readable exception guidance:

- purpose: every typed error documents the caller-facing failure purpose
- reason: failures preserve enough context for local diagnosis
- common fixes: map repeated failures to a small set of operator repairs
- docs_url: point users to this file or a narrower runbook
- repair_hint: state the next command or config change to try

Cost and bounded-operation policy: budget, quota, spend cap, kill switch, and
stop condition evidence must be added before introducing paid or unbounded
network operations.

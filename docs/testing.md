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

## Proof evidence lane

`ops/ci/proof_evidence.sh` runs inside `ops/ci/pr-ci.sh`, so it is part of the
protected `jeryu-intelligence/required` check and blocks merge
(`agent/proof-lanes.toml`). The mirrored workflow runs it only through
`pr-ci.sh`. Every Jankurai tool in it runs on the exact head and every report is
read and gated; nothing is synthesized. The rules are in `ops/ci/proof_baseline.sh`:

- **Baseline.** Protected main's tip as the forge serves it when the gate runs.
  `origin` must be exactly
  `https://git.neverhuman.org/git/jeryu/jeryu-intelligence.git`, both as
  configured and as git resolves it after any `insteadOf` rewrite. Inside a
  GitHub Actions run on github.com, and only there, the lane accepts the
  mirror `https://github.com/neverhuman/jeryu-intelligence` instead and takes
  the mirror's main as its baseline. The forge's own required check never
  does. The lane fetches
  `main` from it and audits that commit in a disposable checkout under
  `target/jeryu-intelligence-proof-baseline.*`, which needs network access to the
  forge. The checkout is created after the tools that scan `.` (copy-code, rust
  witness, proofbind, proofmark, security). No baseline is read from the tree
  under test.
- **Change set.** It starts at the head's fork point from that tip. A head behind
  the tip is judged, not refused. An empty change set (main's own head) passes.
  Deleted paths leave no surface to bind; they are counted in the summary
  (`deleted_path_count`), and a deletion-only head still gets every whole-tree
  check.
- **Ratchet and floor.** The floor is this repository's own integer
  `minimum_score` in `agent/audit-policy.toml`. The head's whole tree must reach
  it and score at least main's, with no new cap or hard finding and no higher
  migration liability. The result depends on main's tip at gate time: rebase
  and rerun when main moves past a score the head does not reach.
- **Audit policy.** A head may amend `agent/audit-policy.toml`, but it may not
  lower the floor. It is then judged under main's policy against main's
  baseline, and under its own policy against main's tree audited with that
  policy.
- **Leftovers.** Each run holds an exclusive `flock` on its checkout's `.lock`.
  The next run removes checkouts older than ten minutes whose lock nobody
  holds, and unlocked ones older than two hours.
- **Security.** `tools/security-lane.sh` is strict: when gitleaks (or
  actionlint, with workflows present) is not installed it fails instead of
  recording a scan that never ran.

The lane writes its ratchet report to `target/jankurai/proof-ratchet-score.*`
and its summary to `target/jankurai/proof-evidence-summary.json`. It leaves
`score.sh`'s `target/jankurai/repo-score.*` in place.

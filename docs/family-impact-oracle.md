# Family impact oracle — research, 2026-10-15

Recommendation: build an advisory, commit-addressed read model on the forge,
reusing codegraph's local impact analysis. Let the gate host supply isolated
contract-export receipts when necessary. Do not put family discovery, network
fetching, or sibling builds into the required audit. This is a proposal; no
family oracle, endpoint, or gate integration is implemented by this note.

## What exists

Repository-relative references below name the owning repository. Inspection
used Intelligence `f540b0aa`, Deploy `fa979283`, and mirror `origin/main` commits:
Core `4e68358c`, Web `5bea7556`, Jira `c5e16c4b`, Tool Finder `45f46ca8`,
Release Ops `c22b1208`. These are snapshots, not a mutually released family.
The motivating internal-wiki page `jeryu-whitepaper-brief` (2026-10-15) was
identified in the task but its contents were not available in these checkouts;
the findings here come from code, not an independent reading of that page.

| Existing surface | Useful evidence and limit |
| --- | --- |
| Intelligence `crates/jeryu-codegraph/src/oracle.rs`, [local oracle contract](codegraph-oracle.md) | `CodeGraphService::query` indexes one materialized workspace, calculates Cargo reverse reachability, attaches governance and an index receipt. Changed paths and crate-level affected symbols do not establish cross-repository symbol usage or behavioral compatibility. |
| Tool Finder `src/scan.rs` | Family scan reads `[[repo]].path/name`, invokes `scan_tool_build_family`, and persists ranked duplicate clusters. Defaults include eight-line windows and at least two repositories. System mode discovers more families. Similarity is a review hint, not a dependency edge; missing directories are skipped by family discovery. |
| Release Ops `repos.manifest.toml` | Sole family membership/release identity authority: eleven required repositories, ten `[[repo]]` rows plus the control plane. Includes hosted transport, required checks and declared tags. Enumerate the control plane too; do not infer membership from directories or the smaller deploy lock. Nested-family references are scope boundaries, not permission to scan other families. |
| Deploy `jeryu-split.lock.toml` | Eight release assembly rows, immutable tag/commit pairs and Web commit/bundle digest. `PENDING_SELF` is not a commit. `local_path` is historical deployment metadata, not a trusted path to open. This lock is not the effective Cargo dependency graph. |
| Cargo manifests, `Cargo.lock`, `.cargo/hosted-gitconfig` | Actual package dependencies, patches, resolved revisions and transport aliases. Deploy's API directly pins Core/readmodel and Jira. Root patches unify transitive sources. Declared manifest tags and assembly-lock rows can differ from effective resolution; report disagreement rather than silently choosing one. |
| Each repository's `agent/generated-zones.toml` | Generated paths, commands and edit policy. Intelligence has both `[[zones]]` and `[[zone]]` for its symbol schema; consumers must handle both. These tables do not consistently name an upstream repository, source revision or mirror relationship. |

Contract ownership is concrete: Core's
`crates/jeryu-readmodel/src/contracts/` feeds `export_contracts`, with a
byte-identity test in `crates/jeryu-readmodel/tests/contract_drift.rs`.
Jira's `crates/jeryu-jira/src/contracts.rs` feeds
`jeryu-jira-export-contracts`; `tests/contract_drift.rs` checks both file set
and bytes. Both exporters accept `TS_RS_EXPORT_DIR`. Web's
`contracts/generated/` contains readmodel and Work DTOs, but its generated-zone
entry names only the readmodel exporter. Web's `test:contracts` runs `tsd`:
type assertions complement, but do not prove, cross-repository byte identity.
An explicit per-exporter output inventory is needed to compare the union
without treating one producer's files as another producer's extra files.

Deploy's `scripts/release/auto-pin.sh` proposes Web commit/digest bumps after
green main; it never merges. `scripts/release/verify-pin-bump.sh` is default-off
and standalone: `docs/pin-bump-fast-lane.md` explicitly says no gate skips a
lane based on it. Neither predicts sibling impact before merge.

## Proposed contract

Input `family-impact/v1`: authorized family repository ID, immutable base and
head SHAs, and a diff bound to those SHAs (including old/new paths, deletions
and renames). The service derives or verifies the diff from Git. Include a
manifest revision and an explicit consumer snapshot vector: each sibling's
resolved main SHA, plus the currently consumed dependency revisions. This
distinguishes “could consume this change” from “currently ships this change.”
Diff text or PR prose must not select repositories, paths or executable commands.

Output: schema/analyzer version, request identity, snapshot vector, input
digests, creation time, completeness (`complete`, `partial`, `unknown`),
unresolved edges, truncation and residual risk; then affected-repository rows:

- Repository and affected package/path; direct or transitive reason chain.
- Carrier: `cargo_pin`, `assembly_pin`, `contract_mirror`, or
  `duplicate_hint`; source/destination repo, package and path, metadata blob
  SHA, field/key, declared tag and resolved commit where applicable.
- Confidence: `confirmed` for an observed carrier or byte comparison,
  `probable` for conservative package/source reachability, `hint` for
  duplication. Confidence describes evidence, not a probability of breakage.
- Separate `impact` (`potential`, `verified_drift`, `unchanged`, `unknown`)
  from `action` (`review`, `bump_after_release`, `regenerate`, `none`,
  `unresolved`). Include before/after hashes and receipt IDs for verified drift.

“Must bump” means the consumer must change the listed pin to adopt the new
release; an internal implementation edit does not force every consumer to
upgrade or prove its current pinned build broken. Before a tag exists return
`bump_after_release` with no invented target tag. Track Cargo declaration,
effective patch, lock resolution and assembly pin independently, grouped into
one adoption plan. A contract copy is an independent carrier even without Cargo.

For example, Core `crates/jeryu-core/src/engine/storage.rs` maps to package
`jeryu-core`: Deploy is a confirmed direct consumer via
`crates/jeryu-api/Cargo.toml` and its effective Cargo lock; its assembly lock
also carries Core. Behavioral impact remains potential, and adopting the fix
requires a later released tag and consumer bump. Traverse the family snapshot
for further consumers rather than claiming this example is exhaustive.
Storage alone does not establish a Web or Jira DTO change.

A readmodel DTO edit makes Core's generated files and Web's mirror probable
impact. A Jira DTO edit similarly reaches Jira's generated output, Web's Work
copies and Deploy's Jira adoption pin. Mark drift verified only after comparing
base/head exports and consumer bytes/file sets. Report pre-existing mismatch
separately from mismatch introduced by the PR. A generated-only edit points
back to its producer and drift proof; filenames alone cannot establish ownership.

## Execution and cost

Build typed reverse edges from manifests and resolved locks, then add explicitly
reviewed producer/output/mirror mappings (the missing metadata). Normalize
known Git source aliases through family authority while retaining original
Cargo identities. Cache by commit and analyzer version. Use local codegraph
to seed changed packages, traverse reverse edges with cycle detection, and
attach existing owner/proof-lane metadata. Duplicate clusters may add hints,
never suppress dependency reachability. Unknown dynamic/API consumers remain
residual risk; incomplete evidence must never become “no affected siblings.”

Planning estimates, not measured PR latency: with eleven repositories, warm
metadata parsing and traversal should take roughly 1–5 CPU seconds, a local
changed-repo index 5–30 seconds, and a cached query under a second. Allow
1–5 minutes for a cold snapshot/index refresh, excluding network transfer.
Warm contract export/comparison may add 10–60 seconds per producer; a cold
Rust build can take several minutes. At 100 PRs/day, 5–30 CPU seconds/PR is
about 8–50 CPU minutes/day, plus exports. No model tokens are necessary.
Measure p50/p95 CPU, wall time, bytes read and cache misses on real PRs before
setting an SLO; these ranges are sizing assumptions, not benchmark evidence.

Bound the first version to the authorized manifest's member set, 10,000 changed
paths, 50,000 edges and a 30-second metadata-query budget; return partial
results with explicit limits instead of dropping rows. Refresh asynchronously
on PR updates, member-main movement, tags and producer receipt changes. Persist
only the latest result per open PR/head plus bounded historical receipts with
a retention policy; deduplicate retries by the complete snapshot digest.

The forge should own this read model because it already knows PR identities,
consumer heads, visibility and pins. Serve cached evidence to the audit and
review UI; retain current required checks regardless of oracle availability.
The gate host should run only allowlisted, versioned exporters in isolated
jobs and publish commit-bound receipts. Never execute generator strings from
PR-controlled TOML on the forge; validate IDs, SHAs and relative paths, reject
symlink escapes, and enforce sibling read permissions before exposing results.

Implement metadata edges and advisory rows first, then exporter/mirror receipts.
Acceptance fixtures should cover storage-only changes, Jira/readmodel exports,
generated-only edits, removed/renamed files, source aliases and patches, cycles,
stale/missing snapshots, pre-existing drift, unauthorized siblings and bounds.
Replay historical PRs against their actual snapshot revisions and compare
recommendations with later pin/mirror changes. Promote no blocking behavior
until coverage and false-negative review support it. This adds the missing
pre-merge family answer while leaving post-merge auto-pin responsible for
mechanical adoption.

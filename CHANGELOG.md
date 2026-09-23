# Changelog

## Unreleased
- Tool-build clusters are ranked by the duplication a shared tool would remove
  (one copy's tokens times the copies past the first), weighted by cross-repo
  spread and by how many domain anchors the window carries, instead of by a
  term that squared the occurrence count. Standard-library plumbing no longer
  crowds out real shared-tool leads on the Intelligence and Shared tools pages.
  The v1 compatibility scan keeps its pinned scores.
- `jeryu-codegraph` storage, oracle and tool-build scan carry unit tests.
- Signing consolidated on the `jeryu-signing` crate: the per-crate `signing`
  re-export modules in `jeryu-review` and `jeryu-autonomy` are gone and their
  duplicated primitive tests with them; `ops/ci/check.sh` now rejects a second
  in-workspace copy of the ed25519/HMAC/digest dependencies.
- v5.0.0 split baseline live on the local forge; merge-to-GitHub mirror verified.

## jeryu-intelligence-v5.0.0-split.0 - 2026-06-11
- MAJOR: first standalone split-family release; the legacy monorepo
  (/home/ubuntu/jeryu) is deprecated and its drift fully reconciled.

## jeryu-intelligence-v4.0.0-split.0

- Initial split-family baseline for `jeryu-intelligence`.

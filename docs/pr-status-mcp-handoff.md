# PR workflow-status MCP prerequisite

Todo: 20261015-131726-fdada1. Parent: 20261012-214150-cefe47.

This change prepares the owning MCP catalog entry `jeryu.pull.workflow_status`
with required `repo` (nonempty string, forge repository identifier) and `number`
(positive unsigned integer). It adds no forge adapter. Without that adapter the
built-in backend returns an error, never an invented workflow status.

## Execution boundary

Keep `ToolBackend::call` synchronous. Deploy's native pull detail handler is
async only at the Axum extraction/response boundary: `web/pulls.rs` builds its
aggregate synchronously in `detail_response`, including `workflow::view` and a
final current-head read. The next slice should extract a synchronous authorized
aggregate read service, returning domain data/errors, and call that service from
both HTTP and MCP. Do not call the HTTP handler, create another runtime, or use
HTTP loopback. Preserve the final head re-read and the workflow's stale-head
semantics; use the complete aggregate rather than reconstructing checks/reviews
inside MCP. Share the existing repository/bot authorization decision between
the two adapters rather than reproducing it in the new tool.

## Publication and remaining work

Deploy currently consumes intelligence tag `jeryu-intelligence-v5.0.0-split.7`,
commit `0c09f438eb71cd5c1b521df26d9404dbecac8bea`. Its MCP transport resolves
calls through the compiled owning catalog before invoking the backend. Adding
only a Deploy descriptor cannot make a new name callable.

After this prerequisite lands, publish intelligence through its normal immutable
release procedure. A subsequent change must pin that published tag and lockfile
commit in Deploy, extract the authorized aggregate service, install exactly this
one RepoRead adapter in discovery/dispatch, and prove native REST/MCP parity,
agent-token discovery/calls within grants, cross-repository denial, stale-head
handling, missing PR/invalid arguments and unsupported calls. Build the consumer
from the published dependency, with no sibling path patches. This unattended
worker is forbidden to push or publish, so it cannot perform that release step.
The end-to-end todo remains incomplete until those consumer checks pass.

The parent todo 20261012-214150-cefe47 retains all repositories/files reads,
other PR tools and mutations, attention, todo and release tools. It can reuse
the synchronous authorized read boundary after the Deploy slice lands. No merge
mutation, broader admin grant, personal-token exception or identity-policy
change belongs in this prerequisite.

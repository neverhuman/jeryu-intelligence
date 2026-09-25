//! MCP JSON-RPC conformance suite (ported from `src/mcp/tests.rs`).
//!
//! Covers: manifest completeness, stdio initialize+tools/list, HTTP
//! initialize -> tools/list -> tools/call -> delete round-trip, concurrent requests
//! on one session, malformed JSON,
//! unknown tool, non-loopback Origin rejection, unknown session, GET-not-allowed,
//! loopback-origin strictness, and the pr_number rename guard.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::json;

use jeryu_mcp::{
    MCP_PROTOCOL_VERSION, McpCore, McpHttpState, McpSessionState, MemoryBackend, ToolBackend,
    mcp_router, tool_manifest,
};

fn backend() -> Arc<MemoryBackend> {
    Arc::new(MemoryBackend::new())
}

async fn spawn_http_server() -> (String, tokio::task::JoinHandle<()>) {
    let state = Arc::new(McpHttpState::new(backend()));
    let app = mcp_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), server)
}

async fn initialize_session(
    client: &reqwest::Client,
    base: &str,
    origin: &str,
    init_body: serde_json::Value,
) -> (reqwest::Response, String) {
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Origin", origin)
        .header("Mcp-Method", "initialize")
        .json(&init_body)
        .send()
        .await
        .unwrap();
    let session = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    (resp, session)
}

fn authenticated_post(
    client: &reqwest::Client,
    base: &str,
    origin: &str,
    session: &str,
    method: &str,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/mcp"))
        .header("Origin", origin)
        .header("Mcp-Session-Id", session)
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .header("Mcp-Method", method)
}

async fn setup_authenticated_session()
-> (String, reqwest::Client, String, tokio::task::JoinHandle<()>) {
    let (base, server) = spawn_http_server().await;
    let client = reqwest::Client::new();
    let origin = base.clone();
    let (_init_resp, session) = initialize_session(
        &client,
        &base,
        &origin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
        }),
    )
    .await;
    (base, client, session, server)
}

#[test]
fn manifest_includes_capability_tools() {
    let manifest = tool_manifest();
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.run_tests")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.fetch_capsule")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.get_ci_run_jobs")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.get_ci_bottlenecks")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.codegraph.query")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.agent_work.start")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.codegraph.tool_build.clusters")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.control_plane.status")
    );
    assert!(
        manifest
            .iter()
            .any(|tool| tool["name"] == "jeryu.runner_fabric.status")
    );
}

#[test]
fn manifest_covers_all_catalog_actions() {
    let manifest = tool_manifest();
    let names: std::collections::BTreeSet<String> = manifest
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(ToString::to_string))
        .collect();

    // The 43-tool catalog (replaces the source's action_registry guardrail).
    let expected = [
        "fetch_capsule",
        "get_system_snapshot",
        "get_ci_run_jobs",
        "get_ci_bottlenecks",
        "explain_blockers",
        "plan_validation",
        "run_tests",
        "propose_patch",
        "race_patches",
        "request_merge",
        "bug_submit",
        "bug_list",
        "bug_show",
        "bug_ready",
        "bug_update",
        "bug_record_attempt",
        "workcell.claim",
        "workcell.status",
        "workcell.repair_live",
        "workcell.export_pr",
        "workcell.release",
        "agent_work.start",
        "agent_work.status",
        "agent_work.control",
        "agent_work.events",
        "agent_work.tail",
        "agent_work.export_pr",
        "code.symbols.search",
        "code.definition",
        "code.impact",
        "code.crate.reverse_deps",
        "code.references",
        "codegraph.query",
        "codegraph.tool_build.status",
        "codegraph.tool_build.clusters",
        "codegraph.tool_build.feedback",
        "tool_finder.clusters",
        "tool_finder.scan",
        "tool_finder.dashboard",
        "tool_registry.summary",
        "control_plane.status",
        "control_plane.priorities",
        "repo_graph.clusters",
        "repo_graph.query",
        "remote.status",
        "artifacts.latest",
        "runner_fabric.status",
    ];
    assert_eq!(
        names.len(),
        47,
        "expected exactly 47 tools, got {}",
        names.len()
    );
    for id in expected {
        assert!(
            names.contains(&format!("jeryu.{id}")),
            "missing MCP tool for catalog action {id}"
        );
    }
}

/// Every tool name `jeryu-deploy`'s `WebMcpBackend` dispatches must exist in the
/// catalog, otherwise `tools/call` rejects it as unknown and `tools/list` hides it.
/// Mirrored by hand from `crates/jeryu-api/src/web/mcp_backend.rs` in `jeryu-deploy`,
/// which is a separate repository and cannot be read from here.
#[test]
fn catalog_covers_every_web_backend_dispatch() {
    let dispatched = [
        "agent_work.start",
        "agent_work.status",
        "agent_work.control",
        "agent_work.events",
        "agent_work.tail",
        "agent_work.export_pr",
        "codegraph.query",
        "code.symbols.search",
        "code.definition",
        "code.impact",
        "code.crate.reverse_deps",
        "code.references",
        "codegraph.tool_build.status",
        "codegraph.tool_build.clusters",
        "codegraph.tool_build.feedback",
        "tool_finder.clusters",
        "tool_finder.scan",
        "tool_finder.dashboard",
        "tool_registry.summary",
        "control_plane.status",
        "control_plane.priorities",
        "repo_graph.clusters",
        "repo_graph.query",
        "remote.status",
        "artifacts.latest",
        "runner_fabric.status",
        "get_system_snapshot",
        "get_ci_run_jobs",
        "get_ci_bottlenecks",
        "explain_blockers",
        "plan_validation",
    ];
    let names: std::collections::BTreeSet<String> = tool_manifest()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(ToString::to_string))
        .collect();
    for id in dispatched {
        assert!(
            names.contains(&format!("jeryu.{id}")),
            "web backend dispatches {id} but the catalog does not define it"
        );
    }
}

#[test]
fn agent_work_tail_schema_matches_dispatch() {
    let manifest = tool_manifest();
    let tool = manifest
        .iter()
        .find(|tool| tool["name"] == "jeryu.agent_work.tail")
        .expect("agent_work.tail is in the manifest");
    let schema = &tool["inputSchema"];
    assert_eq!(schema["required"], json!(["agent_run_id"]));
    assert_eq!(schema["properties"]["agent_run_id"]["type"], "string");
    assert_eq!(schema["properties"]["after_seq"]["type"], "integer");
    assert_eq!(schema["properties"]["limit"]["type"], "integer");
}

#[test]
fn request_merge_uses_pr_number() {
    let manifest = tool_manifest();
    let tool = manifest
        .iter()
        .find(|t| t["name"] == "jeryu.request_merge")
        .expect("request_merge present");
    let required = tool["inputSchema"]["required"]
        .as_array()
        .expect("required array");
    let required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(required.contains(&"pr_number"), "must require pr_number");
    assert_eq!(
        required.iter().filter(|name| **name == "pr_number").count(),
        1
    );
}

#[test]
fn loopback_origin_validation_is_strict() {
    assert!(jeryu_mcp::http::is_loopback_origin("http://127.0.0.1:8899"));
    assert!(jeryu_mcp::http::is_loopback_origin("http://localhost:8899"));
    assert!(jeryu_mcp::http::is_loopback_origin("https://[::1]:8899"));
    assert!(!jeryu_mcp::http::is_loopback_origin("https://example.com"));
    assert!(!jeryu_mcp::http::is_loopback_origin(
        "http://localhost.evil"
    ));
}

#[tokio::test]
async fn stdio_initialize_and_tools_list_work() {
    let core = McpCore::new(backend());
    let mut state = McpSessionState::new();

    let init = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "clientInfo": { "name": "test", "version": "0.1.0" }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(init.len(), 1);
    assert_eq!(init[0]["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    assert_eq!(init[0]["result"]["serverInfo"]["name"], "jeryu");

    let list = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(list.len(), 1);
    assert!(list[0]["result"]["tools"].is_array());
    assert_eq!(list[0]["result"]["tools"].as_array().unwrap().len(), 47);
}

#[tokio::test]
async fn stdio_tools_call_round_trip() {
    let core = McpCore::new(backend());
    let mut state = McpSessionState::new();
    core.handle_line_test(
        &mut state,
        &serde_json::to_string(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
        }))
        .unwrap(),
    )
    .await;

    let call = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.explain_blockers",
                    "arguments": { "entity_type": "merge", "entity_id": 1 }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(call.len(), 1);
    assert!(call[0]["result"]["content"].is_array());
    assert_eq!(call[0]["result"]["isError"], false);
    assert_eq!(call[0]["result"]["structuredContent"]["success"], true);

    let workcell = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.workcell.status",
                    "arguments": { "workcell_id": "wc-17" }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(workcell.len(), 1);
    assert_eq!(workcell[0]["result"]["structuredContent"]["success"], true);
    assert_eq!(
        workcell[0]["result"]["structuredContent"]["data"]["state"],
        "ready"
    );

    let codegraph = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.codegraph.query",
                    "arguments": {
                        "changed_paths": ["crates/jeryu-codegraph/src/lib.rs"],
                        "symbol": "CodeGraph",
                        "crate_name": "jeryu-codegraph"
                    }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(codegraph.len(), 1);
    let structured = &codegraph[0]["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(structured["data"]["schema_version"], "codegraph.query/v1");
    assert_eq!(structured["data"]["definition"]["symbol"], "CodeGraph");

    let agent_events = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.agent_work.events",
                    "arguments": {
                        "agent_run_id": "ar-memory-000001",
                        "after_seq": 0,
                        "limit": 10
                    }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(agent_events.len(), 1);
    let structured = &agent_events[0]["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(structured["data"]["agent_run_id"], "ar-memory-000001");
    assert_eq!(structured["data"]["has_more"], false);

    let tool_clusters = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.codegraph.tool_build.clusters",
                    "arguments": {
                        "repo": "memory",
                        "limit": 5
                    }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(tool_clusters.len(), 1);
    let structured = &tool_clusters[0]["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(
        structured["data"]["clusters"][0]["cluster_id"],
        "toolbuild-memory-0001"
    );

    let feedback = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.codegraph.tool_build.feedback",
                    "arguments": {
                        "cluster_id": "toolbuild-memory-0001",
                        "reason": "fixture boilerplate",
                        "ignored_by": "test"
                    }
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(feedback.len(), 1);
    let structured = &feedback[0]["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(structured["data"]["reason"], "fixture boilerplate");

    let control_plane = core
        .handle_line_test(
            &mut state,
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.control_plane.status",
                    "arguments": {}
                }
            }))
            .unwrap(),
        )
        .await;
    assert_eq!(control_plane.len(), 1);
    let structured = &control_plane[0]["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(
        structured["data"]["schemaVersion"],
        "jeryu.control_plane/v1"
    );
    assert_eq!(structured["data"]["artifacts"]["absenceIsSuccess"], false);
}

#[tokio::test]
async fn http_transport_initializes_and_executes_tools() {
    let (base, client, session, server) = setup_authenticated_session().await;
    let origin = base.clone();

    let list_resp = authenticated_post(&client, &base, &origin, &session, "tools/list")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert!(list_resp.status().is_success());
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    assert!(list_body["result"]["tools"].is_array());

    let call_resp = authenticated_post(&client, &base, &origin, &session, "tools/call")
        .header("Mcp-Name", "jeryu.explain_blockers")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "jeryu.explain_blockers",
                "arguments": { "entity_type": "merge", "entity_id": 1 }
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(call_resp.status().is_success());
    let call_body: serde_json::Value = call_resp.json().await.unwrap();
    assert!(call_body["result"]["content"].is_array());

    let delete_resp = client
        .delete(format!("{base}/mcp"))
        .header("Origin", &origin)
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), StatusCode::NO_CONTENT);

    server.abort();
}

/// Backend that parks one `tools/call` inside the handler until released, so a second
/// request provably arrives while the first still holds the session.
struct GatedBackend {
    inner: MemoryBackend,
    entered: std::sync::mpsc::SyncSender<()>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl ToolBackend for GatedBackend {
    fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        ctx: &jeryu_mcp::backend::McpCallContext,
    ) -> anyhow::Result<jeryu_mcp::ToolResponse> {
        if let Some(release) = self.release.lock().unwrap().take() {
            self.entered.send(()).unwrap();
            release.recv().unwrap();
        }
        self.inner.call(tool, args, ctx)
    }

    fn list(&self) -> Vec<jeryu_mcp::ToolDescriptor> {
        self.inner.list()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_transport_serves_concurrent_requests_on_one_session() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let backend = Arc::new(GatedBackend {
        inner: MemoryBackend::new(),
        entered: entered_tx,
        release: Mutex::new(Some(release_rx)),
    });

    let state = Arc::new(McpHttpState::new(backend));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, mcp_router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let origin = base.clone();
    let client = reqwest::Client::new();
    let (_init, session) = initialize_session(
        &client,
        &base,
        &origin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
        }),
    )
    .await;

    let first = tokio::spawn(
        authenticated_post(&client, &base, &origin, &session, "tools/call")
            .header("Mcp-Name", "jeryu.explain_blockers")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.explain_blockers",
                    "arguments": { "entity_type": "merge", "entity_id": 1 }
                }
            }))
            .send(),
    );

    // The first call is now parked inside the handler, still holding the session.
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();

    let second = tokio::spawn(
        authenticated_post(&client, &base, &origin, &session, "tools/list")
            .json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} }))
            .send(),
    );

    // Give the second request time to reach the handler and queue on the session
    // before the first one finishes; without per-session locking it 404s here.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    release_tx.send(()).unwrap();

    let first = first.await.unwrap().unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = second.await.unwrap().unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second_body["id"], 3);
    assert!(second_body["result"]["tools"].is_array());

    // The session outlives both calls rather than being consumed by either.
    let after = authenticated_post(&client, &base, &origin, &session, "tools/list")
        .json(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::OK);

    server.abort();
}

#[tokio::test]
async fn http_transport_rejects_malformed_json() {
    let (base, server) = spawn_http_server().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Origin", &base)
        .header("Mcp-Method", "initialize")
        .body("not-json")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32700);

    server.abort();
}

#[tokio::test]
async fn http_transport_rejects_unknown_tools() {
    let (base, client, session, server) = setup_authenticated_session().await;
    let origin = base.clone();

    let resp = authenticated_post(&client, &base, &origin, &session, "tools/call")
        .header("Mcp-Name", "jeryu.not_a_real_tool")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "jeryu.not_a_real_tool", "arguments": {} }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32601);

    server.abort();
}

#[tokio::test]
async fn http_transport_rejects_non_loopback_origins() {
    let (base, server) = spawn_http_server().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Origin", "https://example.com")
        .header("Mcp-Method", "initialize")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    server.abort();
}

#[tokio::test]
async fn http_transport_rejects_unknown_sessions() {
    let (base, server) = spawn_http_server().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Origin", &base)
        .header("Mcp-Session-Id", "missing")
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .header("Mcp-Method", "tools/list")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    server.abort();
}

#[tokio::test]
async fn http_get_is_not_enabled() {
    let (base, server) = spawn_http_server().await;

    let resp = reqwest::Client::new()
        .get(format!("{base}/mcp"))
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    server.abort();
}

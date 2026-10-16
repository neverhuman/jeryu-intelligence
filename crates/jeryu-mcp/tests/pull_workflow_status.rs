//! The forge owns authorization and data; a transport without its adapter must fail.
use std::sync::Arc;

use jeryu_mcp::{MCP_PROTOCOL_VERSION, McpCore, McpSessionState, MemoryBackend, tool_manifest};
use serde_json::{Value, json};

#[tokio::test]
async fn workflow_status_catalog_validates_arguments_and_refuses_missing_adapter() {
    let descriptor = tool_manifest()
        .into_iter()
        .find(|tool| tool["name"] == "jeryu.pull.workflow_status")
        .expect("workflow status descriptor");
    assert_eq!(descriptor["annotations"]["readOnlyHint"], true);
    assert_eq!(
        descriptor["inputSchema"]["required"],
        json!(["repo", "number"])
    );

    let core = McpCore::new(Arc::new(MemoryBackend::new()));
    let mut session = McpSessionState::new();
    core.handle_line_test(
        &mut session,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": MCP_PROTOCOL_VERSION,
                "clientInfo": { "name": "test", "version": "1" } }
        })
        .to_string(),
    )
    .await;

    for arguments in [
        json!({}),
        json!({"repo": "owner/repo", "number": 0}),
        json!({"repo": "owner/repo", "number": -1}),
        json!({"repo": "owner/repo", "number": "1"}),
        json!({"repo": "", "number": 1}),
        json!({"repo": 1, "number": 1}),
    ] {
        let response = call(&core, &mut session, arguments).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    let response = call(
        &core,
        &mut session,
        json!({"repo": "owner/repo", "number": 1}),
    )
    .await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(response["result"]["structuredContent"]["success"], false);
    assert!(
        response["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("no authorized forge read adapter")
    );
}

async fn call(core: &McpCore, session: &mut McpSessionState, arguments: Value) -> Value {
    core.handle_line_test(
        session,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "jeryu.pull.workflow_status", "arguments": arguments }
        })
        .to_string(),
    )
    .await
    .remove(0)
}

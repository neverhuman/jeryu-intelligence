//! `session.*` tools (K5): read, drive, and wait on a live agent terminal
//! from MCP. `session_id` names the session; for host-PTY runs today it is the
//! agent run id. MCP cannot stream, so `read` and `wait_for` take a cursor and
//! the client loops. Input is audited by the backend like an attached
//! controller's.

use serde_json::Value;

use super::kind::{ToolDefinition, ToolKind};
use crate::session_terminal::{MAX_INPUT_BYTES, MAX_WAIT_MS, key_bytes};
use crate::tools::schema::*;

pub(super) fn definition(action_id: &str, output_schema: Value) -> Option<ToolDefinition> {
    let id = || ("session_id", string_schema());
    let cursor = || ("after_seq", integer_schema());
    let (title, description, annotations, kind, input_schema) = match action_id {
        "session.read" => (
            "Read session terminal",
            "Read terminal output after a cursor. format=text strips ANSI escapes; raw returns base64 bytes.",
            tool_annotations(true, false, true, false),
            ToolKind::SessionRead,
            object_schema(
                &["session_id"],
                &[
                    id(),
                    cursor(),
                    ("max_bytes", integer_schema()),
                    ("format", enum_schema(&["text", "raw"])),
                ],
            ),
        ),
        "session.send_input" => (
            "Send session input",
            "Type text (or base64 bytes) into the session terminal; submit=true appends Enter.",
            tool_annotations(false, false, false, true),
            ToolKind::SessionSendInput,
            object_schema(
                &["session_id"],
                &[
                    id(),
                    ("text", string_schema()),
                    ("bytes_b64", string_schema()),
                    ("submit", boolean_schema()),
                ],
            ),
        ),
        "session.send_keys" => (
            "Send session keys",
            "Press named keys in order: enter, tab, escape, backspace, up, down, left, right, ctrl-c, ctrl-d, ctrl-l, ctrl-z.",
            tool_annotations(false, false, false, true),
            ToolKind::SessionSendKeys,
            object_schema(
                &["session_id", "keys"],
                &[id(), ("keys", array_schema(string_schema()))],
            ),
        ),
        "session.wait_for" => (
            "Wait for session output",
            "Wait until terminal text after a cursor contains a substring, up to timeout_ms (at most 60000).",
            tool_annotations(true, false, false, false),
            ToolKind::SessionWaitFor,
            object_schema(
                &["session_id", "contains"],
                &[
                    id(),
                    cursor(),
                    ("contains", string_schema()),
                    ("timeout_ms", integer_schema()),
                ],
            ),
        ),
        "session.interrupt" => (
            "Interrupt session",
            "Send an interrupt (SIGINT) to the session's foreground process.",
            tool_annotations(false, true, false, true),
            ToolKind::SessionInterrupt,
            object_schema(&["session_id"], &[id()]),
        ),
        _ => return None,
    };
    Some(ToolDefinition {
        title,
        description,
        annotations,
        input_schema,
        output_schema,
        kind,
    })
}

/// Validate and normalize `session.*` arguments; `None` is invalid params.
pub(super) fn normalize(kind: ToolKind, args: &Value) -> Option<Value> {
    let session_id = args.get("session_id").and_then(Value::as_str)?.to_string();
    if session_id.is_empty() || session_id.len() > 128 {
        return None;
    }
    let after_seq = match args.get("after_seq") {
        None => 0,
        Some(value) => value.as_u64()?,
    };
    let out = match kind {
        ToolKind::SessionRead => {
            let format = args.get("format").and_then(Value::as_str).unwrap_or("text");
            if !matches!(format, "text" | "raw") {
                return None;
            }
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(65_536)
                .clamp(1, 1 << 20);
            serde_json::json!({
                "session_id": session_id, "after_seq": after_seq,
                "max_bytes": max_bytes, "format": format,
            })
        }
        ToolKind::SessionSendInput => {
            let text = args.get("text").and_then(Value::as_str);
            let bytes = args.get("bytes_b64").and_then(Value::as_str);
            // Exactly one of text or bytes_b64.
            if text.is_some() == bytes.is_some() {
                return None;
            }
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            let mut input = match (text, bytes) {
                (Some(text), _) => text.as_bytes().to_vec(),
                (_, Some(encoded)) => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()?
                }
                _ => return None,
            };
            if submit {
                input.push(b'\r');
            }
            if input.is_empty() || input.len() > MAX_INPUT_BYTES {
                return None;
            }
            serde_json::json!({ "session_id": session_id, "bytes_b64": encode(&input) })
        }
        ToolKind::SessionSendKeys => {
            let keys = args.get("keys")?.as_array()?;
            if keys.is_empty() || keys.len() > 64 {
                return None;
            }
            let mut input = Vec::new();
            for key in keys {
                input.extend_from_slice(key_bytes(key.as_str()?)?);
            }
            serde_json::json!({ "session_id": session_id, "bytes_b64": encode(&input) })
        }
        ToolKind::SessionWaitFor => {
            let contains = args.get("contains").and_then(Value::as_str)?;
            if contains.is_empty() || contains.len() > 1024 {
                return None;
            }
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000)
                .min(MAX_WAIT_MS);
            serde_json::json!({
                "session_id": session_id, "after_seq": after_seq,
                "contains": contains, "timeout_ms": timeout_ms,
            })
        }
        ToolKind::SessionInterrupt => serde_json::json!({ "session_id": session_id }),
        _ => return None,
    };
    Some(out)
}

fn encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use serde_json::json;

    use super::*;

    fn bytes(value: &Value) -> Vec<u8> {
        let encoded = value["bytes_b64"].as_str().expect("bytes_b64");
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64")
    }

    #[test]
    fn every_session_tool_has_a_definition_and_a_schema() {
        for id in [
            "session.read",
            "session.send_input",
            "session.send_keys",
            "session.wait_for",
            "session.interrupt",
        ] {
            let def = definition(id, json!({})).unwrap_or_else(|| panic!("{id}"));
            assert_eq!(def.input_schema["required"][0], "session_id", "{id}");
        }
        assert!(definition("session.exec", json!({})).is_none());
    }

    #[test]
    fn send_input_takes_exactly_one_payload_and_can_submit() {
        let typed = normalize(
            ToolKind::SessionSendInput,
            &json!({"session_id": "ar-1", "text": "ls", "submit": true}),
        )
        .expect("valid");
        assert_eq!(bytes(&typed), b"ls\r");
        let raw = normalize(
            ToolKind::SessionSendInput,
            &json!({"session_id": "ar-1", "bytes_b64": "/w=="}),
        )
        .expect("valid");
        assert_eq!(bytes(&raw), [0xff]);
        let both = json!({"session_id": "ar-1", "text": "x", "bytes_b64": "eA=="});
        assert!(normalize(ToolKind::SessionSendInput, &both).is_none());
        let neither = json!({"session_id": "ar-1"});
        assert!(normalize(ToolKind::SessionSendInput, &neither).is_none());
        let huge = json!({"session_id": "ar-1", "text": "x".repeat(MAX_INPUT_BYTES + 1)});
        assert!(normalize(ToolKind::SessionSendInput, &huge).is_none());
    }

    #[test]
    fn keys_wait_and_read_are_bounded() {
        let keys = normalize(
            ToolKind::SessionSendKeys,
            &json!({"session_id": "ar-1", "keys": ["up", "enter", "ctrl-c"]}),
        )
        .expect("valid");
        assert_eq!(bytes(&keys), b"\x1b[A\r\x03");
        let unknown = json!({"session_id": "ar-1", "keys": ["meta-x"]});
        assert!(normalize(ToolKind::SessionSendKeys, &unknown).is_none());

        let wait = normalize(
            ToolKind::SessionWaitFor,
            &json!({"session_id": "ar-1", "contains": "PASS", "timeout_ms": 999_999}),
        )
        .expect("valid");
        assert_eq!(wait["timeout_ms"], MAX_WAIT_MS);
        assert_eq!(wait["after_seq"], 0);

        let read = json!({"session_id": "ar-1", "format": "html"});
        assert!(normalize(ToolKind::SessionRead, &read).is_none());
        let missing = json!({"after_seq": 3});
        assert!(normalize(ToolKind::SessionInterrupt, &missing).is_none());
    }
}

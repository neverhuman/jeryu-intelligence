//! `session.*` tools (K5): start, list, inspect, read, drive, wait on, and
//! stop a live agent session from MCP. `session_id` names the session: a
//! Firecracker session id (`vm-…`) or one of its run ids. `session.attach`
//! returns the existing attach path, the channel id, and a one-time control
//! ticket. MCP does not open that socket. `read` and `wait_for` still take a
//! cursor for a client that cannot. Input is audited by the backend like an
//! attached controller's.

use serde_json::Value;

use super::kind::{ToolDefinition, ToolKind};
use crate::session_terminal::{MAX_INPUT_BYTES, MAX_WAIT_MS, key_bytes};
use crate::tools::schema::*;

/// Agent providers a session may run.
/// `shell` is a session that runs no agent, only its shell: the gate a
/// repository's checks run in, in a VM of its own.
const PROVIDERS: &[&str] = &["claude", "codex", "grok", "shell"];
/// Session states `session.list` filters on.
const LIST_STATES: &[&str] = &["running", "finished", "all"];
/// Longest `session.wait_for` regular expression.
const MAX_PATTERN_BYTES: usize = 1024;
/// Longest task text `session.start` carries.
/// As the forge takes it.
const MAX_TASK_BYTES: usize = 8000;

pub(super) fn definition(action_id: &str, output_schema: Value) -> Option<ToolDefinition> {
    let id = || ("session_id", string_schema());
    let cursor = || ("after_seq", integer_schema());
    let (title, description, annotations, kind, input_schema) = match action_id {
        "session.start" => (
            "Start session",
            "Start an agent session in a sandboxed runner, from the repository's latest main, or from the latest tip of the branch it names. Provider shell starts a session with no agent, only its shell, and takes no task, model or connection. Retrying with the same idempotency_key returns the same session.",
            tool_annotations(false, false, false, true),
            ToolKind::SessionStart,
            object_schema(
                &["repo", "provider"],
                &[
                    ("repo", string_schema()),
                    ("provider", enum_schema(PROVIDERS)),
                    ("model", string_schema()),
                    ("task", string_schema()),
                    ("branch", string_schema()),
                    ("connection_id", string_schema()),
                    ("idempotency_key", string_schema()),
                    (
                        "labels",
                        object_schema(
                            &[],
                            &[("todo_id", string_schema()), ("slot", string_schema())],
                        ),
                    ),
                ],
            ),
        ),
        "session.list" => (
            "List sessions",
            "List the caller's sessions (for a bot, its owner's within its reach), newest first.",
            tool_annotations(true, false, true, false),
            ToolKind::SessionList,
            object_schema(
                &[],
                &[
                    ("repo", string_schema()),
                    ("state", enum_schema(LIST_STATES)),
                    ("provider", enum_schema(PROVIDERS)),
                    ("started_by", string_schema()),
                    ("limit", integer_schema()),
                ],
            ),
        ),
        "session.status" => (
            "Session status",
            "Read a session's state: runner node, generation, lease expiry, source and main revisions, channels, recent control outcomes, and who started it.",
            tool_annotations(true, false, true, false),
            ToolKind::SessionStatus,
            object_schema(&["session_id"], &[id()]),
        ),
        "session.stop" => (
            "Stop session",
            "Stop a session: both its agent and shell channels end and the sandbox is torn down.",
            tool_annotations(false, true, true, false),
            ToolKind::SessionStop,
            object_schema(&["session_id"], &[id()]),
        ),
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
            "Wait until terminal text after a cursor contains a substring, or matches a regular expression, up to timeout_ms (at most 60000). Give exactly one of contains or regex.",
            tool_annotations(true, false, false, false),
            ToolKind::SessionWaitFor,
            object_schema(
                &["session_id"],
                &[
                    id(),
                    cursor(),
                    ("contains", string_schema()),
                    ("regex", string_schema()),
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
        "session.attach" => (
            "Attach session terminal",
            "Return the existing attach WebSocket path, the channel id, and a one-time control ticket bound to the caller, the session, and its generation. MCP does not open the socket. The socket keeps its current frames: JSON tty output, and control messages send_input, inject_prompt, resize_pty, interrupt, and terminate. Pass the ticket once on that upgrade. read, send_input, send_keys, and wait_for stay for a client that cannot open a socket.",
            tool_annotations(false, false, false, true),
            ToolKind::SessionAttach,
            object_schema(&["session_id"], &[id(), cursor()]),
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
    match kind {
        ToolKind::SessionStart => return normalize_start(args),
        ToolKind::SessionList => return normalize_list(args),
        _ => {}
    }
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
            let contains = args.get("contains").and_then(Value::as_str);
            let regex = args.get("regex").and_then(Value::as_str);
            // Exactly one of contains or regex; the backend compiles the regex.
            let (field, pattern) = match (contains, regex) {
                (Some(text), None) => ("contains", text),
                (None, Some(pattern)) => ("regex", pattern),
                _ => return None,
            };
            if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
                return None;
            }
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000)
                .min(MAX_WAIT_MS);
            let mut out = serde_json::json!({
                "session_id": session_id, "after_seq": after_seq, "timeout_ms": timeout_ms,
            });
            out[field] = Value::String(pattern.to_string());
            out
        }
        ToolKind::SessionInterrupt | ToolKind::SessionStatus | ToolKind::SessionStop => {
            serde_json::json!({ "session_id": session_id })
        }
        ToolKind::SessionAttach => serde_json::json!({
            "session_id": session_id,
            "after_seq": after_seq,
        }),
        _ => return None,
    };
    Some(out)
}

/// A short identifier-like value: 1..=max bytes of `[A-Za-z0-9._:/@-]`.
fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
}

fn normalize_start(args: &Value) -> Option<Value> {
    let repo = args.get("repo").and_then(Value::as_str)?;
    if !token(repo, 200) {
        return None;
    }
    let provider = args.get("provider").and_then(Value::as_str)?;
    if !PROVIDERS.contains(&provider) {
        return None;
    }
    let optional = |key: &str, max: usize| -> Option<Option<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => {
                let text = value.as_str()?;
                token(text, max).then(|| Some(text.to_string()))
            }
        }
    };
    let model = optional("model", 128)?;
    let idempotency_key = optional("idempotency_key", 128)?;
    // The forge decides whether the branch exists and is a branch name; this
    // only keeps the argument a short token.
    let branch = optional("branch", 200)?;
    // Which of the owner's connected accounts for the provider, when there
    // is more than one; the forge resolves and checks it.
    let connection_id = optional("connection_id", 128)?;
    let given = |key: &str| args.get(key).is_some_and(|value| !value.is_null());
    if provider == "shell" && (given("task") || given("model") || given("connection_id")) {
        return None;
    }
    let task = match args.get("task") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let text = value.as_str()?;
            if text.is_empty() || text.len() > MAX_TASK_BYTES {
                return None;
            }
            Some(text.to_string())
        }
    };
    let labels = match args.get("labels") {
        None | Some(Value::Null) => serde_json::json!({}),
        Some(value) => {
            let map = value.as_object()?;
            let mut out = serde_json::Map::new();
            for (key, label) in map {
                if !matches!(key.as_str(), "todo_id" | "slot") {
                    return None;
                }
                let text = label.as_str()?;
                if !token(text, 128) {
                    return None;
                }
                out.insert(key.clone(), Value::String(text.to_string()));
            }
            Value::Object(out)
        }
    };
    Some(serde_json::json!({
        "repo": repo, "provider": provider, "model": model, "task": task,
        "branch": branch, "connection_id": connection_id,
        "idempotency_key": idempotency_key, "labels": labels,
    }))
}

fn normalize_list(args: &Value) -> Option<Value> {
    let pick = |key: &str, allowed: &[&str]| -> Option<Option<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => {
                let text = value.as_str()?;
                allowed.contains(&text).then(|| Some(text.to_string()))
            }
        }
    };
    let state = pick("state", LIST_STATES)?.unwrap_or_else(|| "all".to_string());
    let provider = pick("provider", PROVIDERS)?;
    let free = |key: &str| -> Option<Option<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => {
                let text = value.as_str()?;
                token(text, 200).then(|| Some(text.to_string()))
            }
        }
    };
    let repo = free("repo")?;
    let started_by = free("started_by")?;
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 50,
        Some(value) => value.as_u64()?.clamp(1, 200),
    };
    Some(serde_json::json!({
        "repo": repo, "state": state, "provider": provider,
        "started_by": started_by, "limit": limit,
    }))
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
            "session.status",
            "session.stop",
            "session.attach",
        ] {
            let def = definition(id, json!({})).unwrap_or_else(|| panic!("{id}"));
            assert_eq!(def.input_schema["required"][0], "session_id", "{id}");
        }
        let start = definition("session.start", json!({})).expect("start");
        assert_eq!(start.input_schema["required"], json!(["repo", "provider"]));
        let list = definition("session.list", json!({})).expect("list");
        assert_eq!(list.input_schema["required"], json!([]));
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

    #[test]
    fn wait_for_takes_exactly_one_of_contains_or_regex() {
        let regex = normalize(
            ToolKind::SessionWaitFor,
            &json!({"session_id": "vm-ar-1", "regex": "PASS|FAIL"}),
        )
        .expect("valid");
        assert_eq!(regex["regex"], "PASS|FAIL");
        assert!(regex.get("contains").is_none());
        let both = json!({"session_id": "vm-ar-1", "contains": "a", "regex": "b"});
        assert!(normalize(ToolKind::SessionWaitFor, &both).is_none());
        let neither = json!({"session_id": "vm-ar-1"});
        assert!(normalize(ToolKind::SessionWaitFor, &neither).is_none());
        let long = json!({"session_id": "vm-ar-1", "regex": "x".repeat(MAX_PATTERN_BYTES + 1)});
        assert!(normalize(ToolKind::SessionWaitFor, &long).is_none());
    }

    #[test]
    fn start_checks_provider_repo_key_and_labels() {
        let start = normalize(
            ToolKind::SessionStart,
            &json!({"repo": "acme/widgets", "provider": "grok", "model": "grok-4",
                    "task": "fix the flaky test", "idempotency_key": "todo-12-a1",
                    "branch": "nightshift/2026-10-09", "connection_id": "pc_7",
                    "labels": {"todo_id": "12", "slot": "w1"}}),
        )
        .expect("valid");
        assert_eq!(start["provider"], "grok");
        assert_eq!(start["branch"], "nightshift/2026-10-09");
        assert_eq!(start["connection_id"], "pc_7");
        assert_eq!(start["labels"]["slot"], "w1");
        let minimal = normalize(
            ToolKind::SessionStart,
            &json!({"repo": "acme/widgets", "provider": "claude"}),
        )
        .expect("valid");
        assert_eq!(minimal["model"], Value::Null);
        assert_eq!(minimal["branch"], Value::Null);
        assert_eq!(minimal["connection_id"], Value::Null);
        assert_eq!(minimal["labels"], json!({}));
        for bad in [
            json!({"repo": "acme/widgets", "provider": "openai"}),
            json!({"repo": "acme widgets", "provider": "codex"}),
            json!({"provider": "codex"}),
            json!({"repo": "acme/widgets", "provider": "codex", "idempotency_key": "has space"}),
            json!({"repo": "acme/widgets", "provider": "codex", "labels": {"owner": "x"}}),
            json!({"repo": "acme/widgets", "provider": "codex", "task": ""}),
            json!({"repo": "acme/widgets", "provider": "codex", "branch": "has space"}),
            json!({"repo": "acme/widgets", "provider": "codex", "task": "x".repeat(MAX_TASK_BYTES + 1)}),
        ] {
            assert!(normalize(ToolKind::SessionStart, &bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_shell_session_starts_with_no_task_model_or_connection() {
        let shell = normalize(
            ToolKind::SessionStart,
            &json!({"repo": "acme/widgets", "provider": "shell", "branch": "todoq-gate/7",
                    "idempotency_key": "gate-7-a1", "labels": {"todo_id": "7"}}),
        )
        .expect("valid");
        assert_eq!(shell["provider"], "shell");
        assert_eq!(shell["task"], Value::Null);
        for extra in [
            json!({"task": "run the gate"}),
            json!({"model": "m"}),
            json!({"connection_id": "pc_7"}),
        ] {
            let mut args = json!({"repo": "acme/widgets", "provider": "shell"});
            args.as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            assert!(normalize(ToolKind::SessionStart, &args).is_none(), "{args}");
        }
        let list = normalize(ToolKind::SessionList, &json!({"provider": "shell"})).expect("valid");
        assert_eq!(list["provider"], "shell");
    }

    #[test]
    fn list_defaults_and_bounds() {
        let list = normalize(ToolKind::SessionList, &json!({})).expect("valid");
        assert_eq!(list["state"], "all");
        assert_eq!(list["limit"], 50);
        let bounded = normalize(ToolKind::SessionList, &json!({"limit": 10_000})).expect("valid");
        assert_eq!(bounded["limit"], 200);
        let bad = json!({"state": "zombie"});
        assert!(normalize(ToolKind::SessionList, &bad).is_none());
        let stop =
            normalize(ToolKind::SessionStop, &json!({"session_id": "vm-ar-7"})).expect("stop");
        assert_eq!(stop, json!({"session_id": "vm-ar-7"}));
    }

    #[test]
    fn attach_names_a_session_and_a_cursor() {
        let attach = normalize(
            ToolKind::SessionAttach,
            &json!({"session_id": "vm-ar-7", "after_seq": 4}),
        )
        .expect("valid");
        assert_eq!(attach, json!({"session_id": "vm-ar-7", "after_seq": 4}));
        let defaulted =
            normalize(ToolKind::SessionAttach, &json!({"session_id": "vm-ar-7"})).expect("cursor");
        assert_eq!(defaulted["after_seq"], 0);
        assert!(normalize(ToolKind::SessionAttach, &json!({"after_seq": 1})).is_none());
        assert!(normalize(ToolKind::SessionAttach, &json!({"session_id": ""})).is_none());
        assert!(
            normalize(
                ToolKind::SessionAttach,
                &json!({"session_id": "vm-ar-7", "after_seq": -1})
            )
            .is_none()
        );
    }
}

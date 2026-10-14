//! gardenbed-mcp — the MCP surface (M7): let agents ask the garden
//! questions with **no working copy anywhere**.
//!
//! The binary `gardenbed-mcp` serves newline-delimited JSON-RPC 2.0 on
//! stdin/stdout (hand-rolled with `serde_json`, no MCP SDK) with four
//! tools: `log`, `diff`, `blame`, `file` (v0.1.md §1). It opens the
//! arboretum's [`gardenbed_server::Repo`] **in process** over
//! `--root` (the arboretum's data dir: `loam/` + `seedbed.sqlite3`), so
//! every answer comes from the same service layer the CLI's server
//! serves gRPC from — parity with `bed` by construction, and the
//! zero-checkout contract (v0.1.md §M7.2) falls out: nothing here touches
//! a `.bed` directory.
//!
//! Layout:
//!
//! - [`protocol`]: wire shapes — parse, response/error rendering, codes.
//! - [`tools`]: the four tool implementations over [`Repo`].
//! - [`qa`]: the scripted example agent (3-question demo + the ≥20-question
//!   canned QA suite the verification runs).
//!
//! Protocol behavior (see docs/mcp.md for the full reference):
//!
//! | Message | Answer |
//! |---|---|
//! | `initialize` | echo the client's `protocolVersion`, else `2025-06-18`; capabilities `{tools:{}}`; serverInfo |
//! | `notifications/initialized` | nothing (a notification) |
//! | `ping` | empty result |
//! | `tools/list` | the four tools with JSON-Schema input schemas |
//! | `tools/call` | `{content:[{type:"text",text}], isError?}` |
//! | anything else with an id | JSON-RPC error `-32601` |
//! | malformed line | JSON-RPC error `-32700`; the server keeps serving |
//!
//! Diagnostics go to **stderr only** — stdout carries the protocol and
//! nothing else.

pub mod protocol;
pub mod qa;
pub mod tools;

use serde_json::{Value, json};

use gardenbed_server::Repo;

use protocol::Incoming;
use tools::ToolOutcome;

/// Handles one inbound line against `repo`, returning the line to write
/// to stdout (`None` for notifications, which are never answered).
///
/// This is the whole server behavior; the binary runs it in a stdin loop,
/// and the tests call it directly against a fixture [`Repo`].
pub fn handle_line(repo: &Repo, line: &str) -> Option<String> {
    match protocol::parse_line(line) {
        Err(e) => Some(protocol::error_line(&Value::Null, e.code, e.message)),
        Ok(Incoming::Notification { method, .. }) => {
            match method.as_str() {
                // The client's post-initialize handshake acknowledgement.
                "notifications/initialized" => {}
                other => {
                    eprintln!("gardenbed-mcp: ignoring notification {other:?}");
                }
            }
            None
        }
        Ok(Incoming::Request { id, method, params }) => {
            let response = match method.as_str() {
                "initialize" => Ok(initialize_result(&params)),
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({ "tools": tools::descriptors()["tools"] })),
                "tools/call" => match tools::extract_call(&params) {
                    Ok((name, args)) => {
                        let outcome = tools::call_tool(repo, &name, &args);
                        Ok(outcome_json(&outcome))
                    }
                    Err(message) => Err((protocol::INVALID_PARAMS, message)),
                },
                other => Err((
                    protocol::METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                )),
            };
            Some(match response {
                Ok(result) => protocol::result_line(&id, result),
                Err((code, message)) => protocol::error_line(&id, code, message),
            })
        }
    }
}

/// The `initialize` result: negotiate the protocol version (echo the
/// client's when it names one — gardenbed-mcp serves one revision and
/// trusts the client to know it), advertise the tools capability, and
/// introduce the server.
fn initialize_result(params: &Value) -> Value {
    let version = match params.get("protocolVersion") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => protocol::SPEC_PROTOCOL_VERSION.to_owned(),
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": tools::SERVER_NAME, "version": tools::SERVER_VERSION },
    })
}

/// The `tools/call` result shape: one text content block, plus
/// `isError: true` when the tool failed (absent when it succeeded — the
/// field is optional and defaults to false).
fn outcome_json(outcome: &ToolOutcome) -> Value {
    let mut result = json!({
        "content": [ { "type": "text", "text": outcome.text } ],
    });
    if outcome.is_error {
        result["isError"] = json!(true);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolOutcome;

    #[test]
    fn outcome_json_marks_errors() {
        let ok = outcome_json(&ToolOutcome::text("all good"));
        assert_eq!(ok["content"][0]["text"], json!("all good"));
        assert_eq!(ok["content"][0]["type"], json!("text"));
        assert!(ok.get("isError").is_none());

        let bad = outcome_json(&ToolOutcome::error("it went wrong"));
        assert_eq!(bad["isError"], json!(true));
        assert_eq!(bad["content"][0]["text"], json!("it went wrong"));
    }
}

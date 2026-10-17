//! Hand-rolled MCP stdio protocol framing (M7): newline-delimited
//! JSON-RPC 2.0 over stdin/stdout, parsed and rendered with `serde_json`
//! alone — no MCP SDK. The MCP spec's stdio transport is line-delimited
//! JSON-RPC for simple servers; gardenbed-mcp deliberately keeps that
//! minimal shape (one request in, one response out, strictly sequential).
//!
//! Handled methods and their responses live in [`crate`] (`handle_line`);
//! this module owns only the wire shapes: request/notification parsing,
//! response and error rendering, and the JSON-RPC error codes.
//!
//! Notifications (messages without an `id`) are never answered — a
//! malformed line is answered with a parse error bound to `id: null`
//! (its id could not be determined) and the connection keeps serving.

use serde_json::{Value, json};

/// The MCP protocol version gardenbed-mcp advertises when the client does
/// not name one in `initialize` (the 2025-06-18 revision).
pub const SPEC_PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC 2.0 error codes (the spec's reserved range).
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC 2.0 error codes (the spec's reserved range).
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 2.0 error codes (the spec's reserved range).
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 2.0 error codes (the spec's reserved range).
pub const INVALID_PARAMS: i64 = -32602;

/// One parsed inbound line.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A call expecting exactly one response (`id` present, non-null).
    Request {
        /// The echoed-back call id (number or string).
        id: Value,
        /// The method name.
        method: String,
        /// The call's parameters (`{}` when absent).
        params: Value,
    },
    /// A notification — never answered (`id` absent or null).
    Notification {
        /// The method name.
        method: String,
        /// The notification's parameters (`{}` when absent).
        params: Value,
    },
}

/// Why a line could not become a request or notification.
#[derive(Debug, PartialEq)]
pub struct ProtocolError {
    /// The JSON-RPC error code to answer with.
    pub code: i64,
    /// A human-readable explanation.
    pub message: String,
}

impl ProtocolError {
    /// A `-32700` parse error: the line is not valid JSON.
    pub fn parse(message: impl Into<String>) -> Self {
        ProtocolError {
            code: PARSE_ERROR,
            message: message.into(),
        }
    }

    /// A `-32600` invalid request: valid JSON, wrong shape.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        ProtocolError {
            code: INVALID_REQUEST,
            message: message.into(),
        }
    }
}

/// Parses one newline-delimited JSON-RPC 2.0 message.
///
/// Accepts exactly one JSON object per line (JSON-RPC batches are not part
/// of the MCP stdio contract v0.1 serves; a batch is answered as an
/// invalid request rather than half-executed). A missing or null `id`
/// makes the message a notification.
pub fn parse_line(line: &str) -> Result<Incoming, ProtocolError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| ProtocolError::parse(format!("line is not valid JSON: {e}")))?;
    let Value::Object(map) = value else {
        return Err(ProtocolError::invalid_request(
            "a JSON-RPC message must be an object",
        ));
    };
    let jsonrpc_ok = matches!(map.get("jsonrpc"), Some(Value::String(s)) if s == "2.0")
        || !map.contains_key("jsonrpc");
    if !jsonrpc_ok {
        return Err(ProtocolError::invalid_request("jsonrpc must be \"2.0\""));
    }
    let Some(method) = map.get("method") else {
        return Err(ProtocolError::invalid_request(
            "a JSON-RPC message must carry a method",
        ));
    };
    let Value::String(method) = method else {
        return Err(ProtocolError::invalid_request("method must be a string"));
    };
    let params = map.get("params").cloned().unwrap_or_else(|| json!({}));
    let is_request = match map.get("id") {
        None | Some(Value::Null) => false,
        Some(Value::Number(_)) | Some(Value::String(_)) => true,
        Some(_) => {
            return Err(ProtocolError::invalid_request(
                "id must be a number or a string",
            ));
        }
    };
    if is_request {
        Ok(Incoming::Request {
            id: map["id"].clone(),
            method: method.clone(),
            params,
        })
    } else {
        Ok(Incoming::Notification {
            method: method.clone(),
            params,
        })
    }
}

/// Renders a successful response line (no trailing newline).
pub fn result_line(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// Renders an error response line (no trailing newline).
pub fn error_line(id: &Value, code: i64, message: impl AsRef<str>) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.as_ref() },
    })
    .to_string()
}

/// Cap on one stdin protocol line (1 MiB). MCP messages are tiny; a line
/// past this is a broken or hostile client, and the server must answer it
/// with `-32600` and skip it rather than buffering it without bound.
pub const MAX_LINE_LEN: usize = 1 << 20;

/// Reads one newline-terminated (or EOF-terminated) line into `buf`
/// (terminator stripped), never buffering more than [`MAX_LINE_LEN`]
/// bytes: bytes past the cap are counted and discarded.
///
/// Returns `Ok(None)` at EOF with nothing read, `Ok(Some(true))` for a
/// line within the cap, `Ok(Some(false))` for an oversized line (fully
/// skipped, including its remainder up to the newline).
pub fn read_capped_line<R: std::io::BufRead>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut raw: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut got_any = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break; // EOF
        }
        got_any = true;
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if oversized || raw.len() + i > MAX_LINE_LEN {
                    oversized = true;
                } else {
                    raw.extend_from_slice(&available[..i]);
                }
                reader.consume(i + 1);
                break;
            }
            None => {
                let n = available.len();
                if raw.len() + n > MAX_LINE_LEN {
                    oversized = true;
                }
                if !oversized {
                    raw.extend_from_slice(available);
                }
                reader.consume(n);
            }
        }
    }
    if !got_any {
        return Ok(None); // clean EOF: nothing was read
    }
    // A protocol line is JSON; lossy decoding only affects lines that were
    // already rejected by the parser.
    *buf = String::from_utf8_lossy(&raw).into_owned();
    Ok(Some(!oversized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_with_number_and_string_ids() {
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"ping","params":{}}"#;
        match parse_line(line).unwrap() {
            Incoming::Request { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "ping");
                assert_eq!(params, json!({}));
            }
            other => panic!("expected a request, got {other:?}"),
        }
        let line = r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#;
        assert!(matches!(
            parse_line(line).unwrap(),
            Incoming::Request { .. }
        ));
    }

    #[test]
    fn notifications_have_no_id_or_a_null_one() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(matches!(
            parse_line(line).unwrap(),
            Incoming::Notification { .. }
        ));
        let line = r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#;
        assert!(matches!(
            parse_line(line).unwrap(),
            Incoming::Notification { .. }
        ));
    }

    #[test]
    fn malformed_shapes_are_invalid_requests() {
        // Valid JSON of the wrong shape is -32600; broken JSON is -32700
        // (asserted below — the MCP contract requires the distinction).
        for bad in [
            "[]",
            "42",
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":5}"#,
            r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#,
        ] {
            let err = parse_line(bad).unwrap_err();
            assert_eq!(err.code, INVALID_REQUEST, "for {bad:?}");
        }
        // Broken JSON specifically parses as -32700.
        assert_eq!(parse_line("{oops").unwrap_err().code, PARSE_ERROR);
    }

    #[test]
    fn responses_render_round_trippable_json() {
        let out = result_line(&json!(3), json!({ "ok": true }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["id"], json!(3));
        assert_eq!(v["result"]["ok"], json!(true));
        assert!(v.get("error").is_none());

        let out = error_line(&json!(3), METHOD_NOT_FOUND, "no such method");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], json!(METHOD_NOT_FOUND));
        assert!(v.get("result").is_none());
    }
}

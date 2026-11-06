//! JSON Lines plugin protocol, host side (roadmap §6.5).
//!
//! One request per line on the plugin's stdin, one response per line on its
//! stdout. Every request carries a `request_id`; the response must match it.
//! Stderr is diagnostics. Message size is bounded; invalid UTF-8, malformed
//! JSON, unknown methods, duplicate ids, version mismatch and oversized
//! messages are all errors. The host treats every field as untrusted input.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::BufRead;

/// Hard ceiling on one protocol line (bytes).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub const PROTOCOL_NAME: &str = "nawa-plugin";

/// The `hello` handshake request the host sends first.
#[derive(Debug, Serialize)]
pub struct HelloRequest<'a> {
    pub protocol: &'a str,
    pub protocol_version: u32,
    pub request_id: &'a str,
    pub method: &'a str,
    pub params: HelloParams<'a>,
}

#[derive(Debug, Serialize)]
pub struct HelloParams<'a> {
    pub nawa_version: &'a str,
    pub plugin_api: &'a str,
    /// Effective capabilities granted by the system administrator.
    pub granted_capabilities: &'a [String],
}

/// A successful `hello` response. Every field is part of the protocol
/// contract even when the current host does not consume it.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct HelloResponse {
    pub plugin_id: String,
    pub plugin_version: String,
    pub api_version: String,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// The envelope of every response line.
#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub protocol: String,
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A `run` command request (one-shot command execution).
#[derive(Debug, Serialize)]
pub struct RunRequest<'a> {
    pub protocol: &'a str,
    pub protocol_version: u32,
    pub request_id: &'a str,
    pub method: &'a str,
    pub params: RunParams<'a>,
}

#[derive(Debug, Serialize)]
pub struct RunParams<'a> {
    pub command: &'a str,
    pub args: &'a [String],
}

/// A post-transaction observer request.
#[derive(Debug, Serialize)]
pub struct EventRequest<'a> {
    pub protocol: &'a str,
    pub protocol_version: u32,
    pub request_id: &'a str,
    pub method: &'a str,
    pub params: EventParams<'a>,
}

#[derive(Debug, Serialize)]
pub struct EventParams<'a> {
    pub event_type: &'a str,
    pub payload: &'a serde_json::Value,
}

/// Serialize one request line.
pub fn encode_request<T: Serialize>(req: &T) -> Result<String> {
    let line = serde_json::to_string(req).context("serialize plugin request")?;
    if line.len() > MAX_MESSAGE_BYTES {
        bail!("plugin request exceeds {} bytes", MAX_MESSAGE_BYTES);
    }
    Ok(line)
}

/// Read one bounded line from the plugin's stdout and parse the envelope.
pub fn read_response(reader: &mut impl BufRead) -> Result<Envelope> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // read byte-by-byte up to the bound: a line longer than the limit must
    // fail instead of being buffered into memory first
    loop {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_MESSAGE_BYTES {
            bail!("plugin response exceeds {} bytes", MAX_MESSAGE_BYTES);
        }
    }
    if line.is_empty() {
        bail!("plugin closed its stdout without answering");
    }
    let text = std::str::from_utf8(&line).context("plugin response is not valid UTF-8")?;
    let env: Envelope = serde_json::from_str(text)
        .with_context(|| format!("plugin response is not valid protocol JSON: {text:?}"))?;
    if env.protocol != PROTOCOL_NAME {
        bail!("plugin response announces protocol {:?}, expected {PROTOCOL_NAME:?}", env.protocol);
    }
    if env.protocol_version != 1 {
        bail!("plugin response uses protocol version {}", env.protocol_version);
    }
    Ok(env)
}

/// Validate that the envelope answers `request_id` and carries a result.
pub fn expect_result(env: Envelope, request_id: &str) -> Result<serde_json::Value> {
    if env.request_id != request_id {
        bail!(
            "plugin answered request {:?} but the host asked {:?}",
            env.request_id, request_id
        );
    }
    if !env.ok {
        bail!(
            "plugin reported an error: {}",
            env.error.unwrap_or_else(|| "(no message)".into())
        );
    }
    env.result.ok_or_else(|| anyhow::anyhow!("plugin response has no result"))
}

/// Parse a validated `hello` result value.
pub fn parse_hello(result: serde_json::Value) -> Result<HelloResponse> {
    serde_json::from_value(result).context("plugin hello result has unexpected fields")
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hello_line() -> String {
        encode_request(&HelloRequest {
            protocol: PROTOCOL_NAME,
            protocol_version: 1,
            request_id: "1",
            method: "hello",
            params: HelloParams {
                nawa_version: "1.0.0",
                plugin_api: "1",
                granted_capabilities: &[],
            },
        })
        .unwrap()
    }

    #[test]
    fn handshake_round_trip() {
        let req = hello_line();
        assert!(req.contains("\"method\":\"hello\""));
        let resp = String::from(
            "{\"protocol\":\"nawa-plugin\",\"protocol_version\":1,\"request_id\":\"1\",\"ok\":true,\"result\":{\"plugin_id\":\"demo\",\"plugin_version\":\"1.0\",\"api_version\":\"1\",\"commands\":[\"demo-test\"],\"capabilities\":[\"history.read\"]}}",
        );
        let mut cursor = Cursor::new(resp.into_bytes());
        let env = read_response(&mut cursor).unwrap();
        let result = expect_result(env, "1").unwrap();
        let hello = parse_hello(result).unwrap();
        assert_eq!(hello.plugin_id, "demo");
        assert_eq!(hello.commands, vec!["demo-test".to_string()]);
    }

    #[test]
    fn wrong_request_id_and_not_ok_are_rejected() {
        let resp = "{\"protocol\":\"nawa-plugin\",\"protocol_version\":1,\"request_id\":\"9\",\"ok\":true,\"result\":{}}";
        let mut cursor = Cursor::new(resp.as_bytes().to_vec());
        let env = read_response(&mut cursor).unwrap();
        assert!(expect_result(env, "1").is_err(), "request id mismatch");

        let resp = "{\"protocol\":\"nawa-plugin\",\"protocol_version\":1,\"request_id\":\"1\",\"ok\":false,\"error\":\"boom\"}";
        let mut cursor = Cursor::new(resp.as_bytes().to_vec());
        let env = read_response(&mut cursor).unwrap();
        let err = expect_result(env, "1").unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
    }

    #[test]
    fn garbage_is_rejected() {
        for bad in ["", "not json", "{\"protocol\":\"other\",\"protocol_version\":1,\"request_id\":\"1\",\"ok\":true}",
                    "{\"protocol\":\"nawa-plugin\",\"protocol_version\":2,\"request_id\":\"1\",\"ok\":true}"] {
            let mut cursor = Cursor::new(bad.as_bytes().to_vec());
            assert!(read_response(&mut cursor).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn oversized_line_is_rejected() {
        let huge = "x".repeat(MAX_MESSAGE_BYTES + 10);
        let mut cursor = Cursor::new(huge.into_bytes());
        assert!(read_response(&mut cursor).unwrap_err().to_string().contains("exceeds"));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut data = vec![0xff, 0xfe, b'\n'];
        let mut cursor = Cursor::new(&mut data);
        assert!(read_response(&mut cursor).is_err());
    }
}

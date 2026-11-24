//! Control protocol: one JSON object per line, one request per connection.
//!
//! ```text
//! -> {"subsystem":"services","verb":"restart","args":["heartbeat"]}
//! <- {"ok":true,"data":{...}}
//! <- {"ok":false,"error":"permission denied: ..."}
//! ```
//!
//! Subsystems today: `system`, `strata`, `services`, `boot`, `overlay`,
//! `locale`. New ones (a Windows stratum manager, a D-Bus/logind shim for
//! Plasma) register in the engine under a new name; amctl only needs a new
//! subcommand that sends `{"subsystem":"<name>",...}`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub subsystem: String,
    pub verb: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            data,
            error: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: Value::Null,
            error: Some(msg.into()),
        }
    }
}

/// Longest a single request may take (starting a stratum can take a while).
pub const TIMEOUT: Duration = Duration::from_secs(90);
/// Requests bigger than this are rejected.
pub const MAX_LINE: usize = 64 * 1024;

/// Send one request to the engine. `Err` = the engine is not reachable
/// (amctl then falls back to what it can do offline).
pub fn call(socket: &str, req: &Request) -> std::io::Result<Response> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(std::io::Error::other("the engine closed the connection"));
    }
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(r#"{"subsystem":"system","verb":"status"}"#).unwrap();
        assert!(r.args.is_empty());
        let ok = serde_json::to_string(&Response::ok(serde_json::json!({"a":1}))).unwrap();
        assert_eq!(ok, r#"{"ok":true,"data":{"a":1}}"#);
        let e = serde_json::to_string(&Response::err("nope")).unwrap();
        assert_eq!(e, r#"{"ok":false,"error":"nope"}"#);
    }
}

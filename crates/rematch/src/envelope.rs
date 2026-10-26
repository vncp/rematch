//! One JSON envelope per invocation on stdout (DESIGN §2). `--text` renders `data`.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub ok: bool,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The operation was cancelled (SIGINT/SIGTERM while a child ran): exit 130.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

/// Command-line usage error discovered after parsing: exit 2.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Usage(pub String);

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CANCELLED: i32 = 130;

impl Envelope {
    pub fn ok(operation: &str, data: Value) -> Envelope {
        Envelope { ok: true, operation: operation.into(), data: Some(data), error: None }
    }

    pub fn err(operation: &str, error: impl Into<String>) -> Envelope {
        Envelope { ok: false, operation: operation.into(), data: None, error: Some(error.into()) }
    }

    /// Map a command result to (envelope, exit code).
    pub fn from_result(operation: &str, r: anyhow::Result<Value>) -> (Envelope, i32) {
        match r {
            Ok(v) => (Envelope::ok(operation, v), EXIT_OK),
            Err(e) => {
                let code = if e.downcast_ref::<Cancelled>().is_some() {
                    EXIT_CANCELLED
                } else if e.downcast_ref::<Usage>().is_some() {
                    EXIT_USAGE
                } else {
                    EXIT_ERR
                };
                (Envelope::err(operation, format!("{e:#}")), code)
            }
        }
    }

    /// Print exactly one line (JSON) or the human rendering with `--text`.
    pub fn print(&self, text: bool) {
        if text {
            println!("{}", self.render_text());
        } else {
            println!("{}", serde_json::to_string(self).unwrap_or_else(|_| "{\"ok\":false}".into()));
        }
    }

    pub fn render_text(&self) -> String {
        if !self.ok {
            return format!("rematch {}: error: {}", self.operation, self.error.as_deref().unwrap_or("unknown"));
        }
        let Some(data) = &self.data else { return format!("rematch {}: ok", self.operation) };
        if let Some(lines) = data.get("lines").and_then(Value::as_array) {
            let mut out: Vec<String> = lines.iter().map(|l| l.as_str().map(str::to_string).unwrap_or_else(|| l.to_string())).collect();
            if out.is_empty() {
                out.push(format!("rematch {}: nothing to report", self.operation));
            }
            return out.join("\n");
        }
        serde_json::to_string_pretty(data).unwrap_or_default()
    }
}

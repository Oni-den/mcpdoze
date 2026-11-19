// SPDX-License-Identifier: GPL-3.0-or-later
//! Replays a `mcpdoze debug export` bundle against an in-process broker.
//!
//! [`from_bundle`] reads the `## Replay` JSON fence of an export. Each
//! [`Scenario`] carries the `mock-mcp-stdio --script` steps for one failure
//! kind and the recorded request shape. [`Scenario::run`] starts the mock with
//! that script, sends a synthetic request with the same method, string
//! lengths, integer digits and string classes through a router with failure
//! diagnostics on, and returns the kinds the broker recorded, so a test can
//! assert the recorded `kind` shows up again.
//!
//! Limit: this reproduces the symptom the broker saw (framing, timing, exit),
//! not the bug inside the server that caused it.

use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::diag::{Diag, DiagConfig, Level};
use mcpdoze_core::router::Router;
use serde_json::{json, Map, Value};

use crate::MockServerSpec;

/// Why a bundle could not be loaded or a scenario could not run.
#[derive(Debug)]
pub struct ReplayError(pub String);

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReplayError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ReplayError> {
    Err(ReplayError(msg.into()))
}

/// The scenarios of one export.
#[derive(Debug, Clone)]
pub struct Replay {
    scenarios: Vec<Scenario>,
}

/// One recorded failure kind with its mock script and request shape.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The failure kind the record carried.
    pub expected_kind: String,
    /// Request method of the record (`tools/call` when absent).
    pub method: String,
    /// Recorded request size in bytes, when known.
    pub bytes: Option<u64>,
    /// Recorded argument shape (`ShapeRoot` JSON), when known.
    pub shape: Value,
    /// The `--script` steps, `None` when the kind has no mock reproduction.
    pub script: Option<Value>,
}

/// Loads a bundle: the first JSON fence after the `## Replay` heading.
pub fn from_bundle(path: impl AsRef<std::path::Path>) -> Result<Replay, ReplayError> {
    let text = std::fs::read_to_string(path.as_ref())
        .map_err(|e| ReplayError(format!("read {}: {e}", path.as_ref().display())))?;
    from_text(&text)
}

/// Like [`from_bundle`] over the export text.
pub fn from_text(text: &str) -> Result<Replay, ReplayError> {
    let Some(at) = text.find("## Replay") else {
        return err("no Replay section");
    };
    let rest = &text[at..];
    let Some(open) = rest.find("```json") else {
        return err("no json fence in the Replay section");
    };
    let body = &rest[open + "```json".len()..];
    let Some(close) = body.find("\n```") else {
        return err("unterminated json fence");
    };
    let doc: Value = serde_json::from_str(&body[..close])
        .map_err(|e| ReplayError(format!("replay json: {e}")))?;
    let Some(list) = doc["scenarios"].as_array() else {
        return err("replay json has no scenarios array");
    };
    let scenarios = list
        .iter()
        .map(|s| Scenario {
            expected_kind: s["expected_kind"].as_str().unwrap_or("").to_owned(),
            method: s["request"]["method"]
                .as_str()
                .unwrap_or("tools/call")
                .to_owned(),
            bytes: s["request"]["bytes"].as_u64(),
            shape: s["request"]["shape"].clone(),
            script: s.get("script").filter(|v| !v.is_null()).cloned(),
        })
        .collect();
    Ok(Replay { scenarios })
}

impl Replay {
    /// All scenarios, in export order.
    pub fn scenarios(&self) -> &[Scenario] {
        &self.scenarios
    }

    /// The scenario recorded for `kind`.
    pub fn scenario(&self, kind: &str) -> Option<&Scenario> {
        self.scenarios.iter().find(|s| s.expected_kind == kind)
    }
}

/// A string of `len` bytes in class `cls`.
fn synth_string(cls: Option<&str>, len: usize) -> String {
    let (prefix, fill) = match cls {
        Some("url") => ("https://example.invalid/", 'a'),
        Some("path") => ("/tmp/example/", 'a'),
        Some("json") => ("{", 'a'),
        Some("multiline") => ("a\n", 'a'),
        Some("unicode") => ("\u{e9}", '\u{e9}'),
        _ => ("", 'a'),
    };
    let mut s = String::new();
    if len >= prefix.len() {
        s.push_str(prefix);
    }
    let w = fill.len_utf8();
    while s.len() + w <= len {
        s.push(fill);
    }
    while s.len() < len {
        s.push('a');
    }
    if cls == Some("json") && len >= 2 {
        s.pop();
        s.push('}');
    }
    s
}

fn synth(node: &Value) -> Value {
    match node["t"].as_str().unwrap_or("null") {
        "bool" => json!(node["v"].as_bool().unwrap_or(false)),
        "int" => {
            let d = node["digits"].as_u64().unwrap_or(1).clamp(1, 18) as u32;
            json!(10u64.pow(d - 1))
        }
        "float" => json!(1.5),
        "string" => Value::String(synth_string(
            node["cls"].as_str(),
            node["len"].as_u64().unwrap_or(0) as usize,
        )),
        "array" => {
            let n = node["n"].as_u64().unwrap_or(0) as usize;
            let items: Vec<Value> = node["items"].as_array().cloned().unwrap_or_default();
            Value::Array(
                (0..n.min(1000))
                    .map(|i| {
                        items
                            .get(i)
                            .or(items.last())
                            .map(synth)
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            )
        }
        "object" => object(&node["keys"]),
        _ => Value::Null,
    }
}

fn object(keys: &Value) -> Value {
    let mut m = Map::new();
    if let Some(k) = keys.as_object() {
        for (name, node) in k {
            m.insert(name.clone(), synth(node));
        }
    }
    Value::Object(m)
}

impl Scenario {
    /// Tool named by the script step, `echo` when none.
    fn tool(&self) -> String {
        self.script
            .as_ref()
            .and_then(|s| s.as_array())
            .and_then(|a| a.iter().find_map(|st| st["on"]["tool"].as_str()))
            .unwrap_or("echo")
            .to_owned()
    }

    /// Synthetic arguments with the recorded types, lengths and classes.
    pub fn synthetic_arguments(&self) -> Value {
        // A top-level object is a plain map; anything else is one node.
        if self.shape.is_null() {
            json!({})
        } else if self.shape.get("t").is_some_and(Value::is_string) {
            synth(&self.shape)
        } else {
            object(&self.shape)
        }
    }

    /// The mock server configured with this scenario's script, written to
    /// `script_path`.
    pub fn mock_spec(&self, script_path: &std::path::Path) -> Result<MockServerSpec, ReplayError> {
        let Some(script) = &self.script else {
            return err(format!("kind {} has no mock script", self.expected_kind));
        };
        std::fs::write(script_path, script.to_string())
            .map_err(|e| ReplayError(format!("write script: {e}")))?;
        Ok(MockServerSpec::new()
            .tools(&[self.tool().as_str()])
            .script(script_path))
    }

    /// Runs the synthetic request through an in-process broker with failure
    /// diagnostics on and returns the recorded kinds (in log order, repeats
    /// kept). `mock_bin` is the `mock-mcp-stdio` path
    /// (`env!("CARGO_BIN_EXE_mock-mcp-stdio")` in an integration test).
    pub async fn run(&self, mock_bin: &str) -> Result<Vec<String>, ReplayError> {
        let dir = tempfile::tempdir().map_err(|e| ReplayError(e.to_string()))?;
        let mock = self.mock_spec(&dir.path().join("script.json"))?;
        let start = if self.expected_kind.starts_with("init_") {
            Duration::from_millis(800)
        } else {
            Duration::from_secs(20)
        };
        let spec = BackendSpec::new(mock_bin, mock.args())
            .start_timeout(start)
            .call_timeout(if self.expected_kind == "frame_oversize" {
                Duration::from_secs(25)
            } else {
                Duration::from_secs(3)
            });
        let diag = Diag::new(DiagConfig::new(dir.path().join("diag")));
        let router = Router::new();
        router.set_diag(diag.clone(), Level::Trace);
        router
            .register("srv", "p", spec)
            .map_err(|e| ReplayError(format!("register: {e:?}")))?;
        let session = router.create_session("2025-11-25");
        let msg = if self.method == "tools/call" {
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":format!("p__{}", self.tool()),"arguments":self.synthetic_arguments()}})
        } else {
            json!({"jsonrpc":"2.0","id":1,"method":self.method,"params":{}})
        };
        let _ = tokio::time::timeout(Duration::from_secs(40), router.handle(&session, msg)).await;
        let mut kinds = Vec::new();
        for _ in 0..50 {
            diag.flush(Duration::from_secs(5));
            kinds = std::fs::read_to_string(diag.file())
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter_map(|r| r["kind"].as_str().map(str::to_owned))
                .collect();
            if kinds.contains(&self.expected_kind) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(kinds)
    }
}

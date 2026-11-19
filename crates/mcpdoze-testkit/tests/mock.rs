// SPDX-License-Identifier: GPL-3.0-or-later
use mcpdoze_testkit::{notification, parse_line, request, MockHttpSpec, MockServerSpec};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const HTTP_BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-http");

struct Mock {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Mock {
    fn start(spec: MockServerSpec) -> Mock {
        let mut child = spec.program(BIN).command().spawn().unwrap();
        let stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        // Hard timeout: kill after 20 s no matter what.
        let pid = child.id();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            kill(pid);
        });
        Mock { child, stdin, rx }
    }
    fn send(&mut self, line: String) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{line}").unwrap();
        s.flush().unwrap();
    }
    fn line(&self) -> String {
        self.rx
            .recv_timeout(Duration::from_secs(45))
            .expect("no output")
    }
    fn msg(&self) -> Value {
        parse_line(&self.line()).expect("json")
    }
    fn handshake(&mut self) -> Value {
        self.send(request(
            1,
            "initialize",
            json!({"protocolVersion":"2025-11-25"}),
        ));
        let r = self.msg();
        self.send(notification("notifications/initialized", json!({})));
        r
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kill(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

#[test]
fn handshake_and_ping() {
    let mut m = Mock::start(MockServerSpec::new().name("alpha"));
    let r = m.handshake();
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(r["result"]["serverInfo"]["name"], "alpha");
    m.send(request(2, "ping", json!({})));
    assert_eq!(m.msg()["id"], 2);
    // echoes requested version
    m.send(request(
        3,
        "initialize",
        json!({"protocolVersion":"2024-11-05"}),
    ));
    assert_eq!(m.msg()["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn tools_list_and_call() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["a", "b"]));
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    let r = m.msg();
    let names: Vec<_> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    m.send(request(
        3,
        "tools/call",
        json!({"name":"b","arguments":{"x":1}}),
    ));
    let r = m.msg();
    assert_eq!(r["result"]["structuredContent"]["tool"], "b");
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"x\":1"));
}

#[test]
fn concurrent_slow_calls_finish_out_of_order() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["slow", "echo"]).delay_ms(800));
    m.handshake();
    m.send(request(
        10,
        "tools/call",
        json!({"name":"slow","arguments":{}}),
    ));
    m.send(request(
        11,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.msg()["id"], 11);
    assert_eq!(m.msg()["id"], 10);
}

#[test]
fn crash_after_exits_with_code_3_without_reply() {
    let mut m = Mock::start(MockServerSpec::new().crash_after(1));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.msg()["id"], 2);
    m.send(request(
        3,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    let status = m.child.wait().unwrap();
    assert_eq!(status.code(), Some(3));
    assert!(m.rx.recv_timeout(Duration::from_secs(5)).is_err());
}

#[test]
fn garbage_line_precedes_first_message() {
    let mut m = Mock::start(MockServerSpec::new().garbage_on_start());
    assert!(parse_line(&m.line()).is_none());
    assert_eq!(m.handshake()["id"], 1);
}

#[test]
fn list_changed_adds_extra_tool() {
    let mut m = Mock::start(MockServerSpec::new().list_changed_after(1));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    let mut saw_changed = false;
    let mut saw_reply = false;
    for _ in 0..2 {
        let r = m.msg();
        if r["method"] == "notifications/tools/list_changed" {
            saw_changed = true;
        } else if r["id"] == 2 {
            saw_reply = true;
        }
    }
    assert!(saw_changed && saw_reply);
    m.send(request(3, "tools/list", json!({})));
    let r = m.msg();
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 2);
}

#[test]
fn pid_and_spawn_child() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["pid", "spawn_child"]));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"pid","arguments":{}}),
    ));
    let pid = m.msg()["result"]["structuredContent"]["pid"]
        .as_u64()
        .unwrap();
    assert_eq!(pid as u32, m.child.id());
    m.send(request(
        3,
        "tools/call",
        json!({"name":"spawn_child","arguments":{}}),
    ));
    let cpid = m.msg()["result"]["structuredContent"]["pid"]
        .as_u64()
        .unwrap();
    assert_ne!(cpid, pid);
    kill(cpid as u32);
}

#[test]
fn sampling_roundtrip() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["ask"]).sampling_request());
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"ask","arguments":{}}),
    ));
    let req = m.msg();
    assert_eq!(req["method"], "sampling/createMessage");
    m.send(mcpdoze_testkit::response(
        req["id"].clone(),
        json!({"role":"assistant"}),
    ));
    let r = m.msg();
    assert_eq!(r["id"], 2);
    assert_eq!(
        r["result"]["structuredContent"]["reply"]["role"],
        "assistant"
    );
}

#[test]
fn exits_cleanly_when_stdin_closes() {
    let mut m = Mock::start(MockServerSpec::new());
    m.handshake();
    drop(m.stdin.take());
    assert_eq!(m.child.wait().unwrap().code(), Some(0));
}

#[test]
fn exit_code_on_trigger_tool() {
    let mut m = Mock::start(MockServerSpec::new().exit_code(7));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"exit","arguments":{}}),
    ));
    assert_eq!(m.child.wait().unwrap().code(), Some(7));
    assert!(m.rx.recv_timeout(Duration::from_secs(5)).is_err());
}

#[test]
fn exit_after_n_calls_with_exit_code() {
    let mut m = Mock::start(MockServerSpec::new().exit_after(1).exit_code(5));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.msg()["id"], 2);
    m.send(request(
        3,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.child.wait().unwrap().code(), Some(5));
}

#[test]
fn oversize_response_is_one_line_over_16_mib() {
    let mut m = Mock::start(MockServerSpec::new().oversize_response());
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    let line = m.line();
    assert!(line.len() > 16 * 1024 * 1024);
    assert_eq!(parse_line(&line).unwrap()["id"], 2);
}

#[test]
fn string_ids_echo_request_ids_as_strings() {
    let mut m = Mock::start(MockServerSpec::new().string_ids());
    assert_eq!(m.handshake()["id"], "1");
    m.send(request(2, "ping", json!({})));
    assert_eq!(m.msg()["id"], "2");
}

#[test]
fn content_length_framing_precedes_each_message() {
    let mut m = Mock::start(MockServerSpec::new().content_length_framing());
    m.send(request(1, "ping", json!({})));
    let header = m.line();
    let len: usize = header
        .strip_prefix("Content-Length: ")
        .expect("header line")
        .parse()
        .unwrap();
    assert_eq!(m.line(), "");
    let body = m.line();
    assert_eq!(body.len(), len);
    assert_eq!(parse_line(&body).unwrap()["id"], 1);
}

#[test]
fn init_error_replies_with_json_rpc_error() {
    let mut m = Mock::start(MockServerSpec::new().init_error());
    let r = m.handshake();
    assert_eq!(r["error"]["code"], -32603);
    assert!(r.get("result").is_none());
}

#[test]
fn init_version_overrides_requested_version() {
    let mut m = Mock::start(MockServerSpec::new().init_version("2099-01-01"));
    let r = m.handshake();
    assert_eq!(r["result"]["protocolVersion"], "2099-01-01");
}

#[test]
fn pretty_json_spans_multiple_lines() {
    let mut m = Mock::start(MockServerSpec::new().pretty_json());
    m.send(request(1, "ping", json!({})));
    let first = m.line();
    assert_eq!(first, "{");
    assert!(parse_line(&first).is_none());
    let mut text = first;
    loop {
        let l = m.line();
        text.push('\n');
        text.push_str(&l);
        if l == "}" {
            break;
        }
    }
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["id"], 1);
}

#[test]
fn stderr_leak_prints_env_value_and_arguments() {
    let spec = MockServerSpec::new().stderr_leak();
    let mut cmd = spec.program(BIN).command();
    cmd.env("MCPDOZE_TEST_CANARY", "env-canary-7731")
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut err = child.stderr.take().unwrap();
    writeln!(
        stdin,
        "{}",
        request(1, "initialize", json!({"protocolVersion":"2025-11-25"}))
    )
    .unwrap();
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    writeln!(
        stdin,
        "{}",
        request(
            2,
            "tools/call",
            json!({"name":"echo","arguments":{"query":"arg-canary-4412"}})
        )
    )
    .unwrap();
    line.clear();
    out.read_line(&mut line).unwrap();
    assert_eq!(parse_line(&line).unwrap()["id"], 2);
    drop(stdin);
    let mut stderr = String::new();
    err.read_to_string(&mut stderr).unwrap();
    child.wait().unwrap();
    assert!(stderr.contains("env-canary-7731"));
    assert!(stderr.contains("arg-canary-4412"));
}

/// Writes a `--script` file into `dir` and returns its path.
fn write_script(dir: &tempfile::TempDir, steps: Value) -> PathBuf {
    let p = dir.path().join("script.json");
    std::fs::write(&p, steps.to_string()).unwrap();
    p
}

#[test]
fn script_stdout_raw_then_respond() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_script(
        &dir,
        json!([{"on":{"method":"tools/list"},
            "do":[{"stdout_raw":"not json"},{"delay_ms":20},"respond"]}]),
    );
    let mut m = Mock::start(MockServerSpec::new().script(path));
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    assert_eq!(m.line(), "not json");
    assert_eq!(m.msg()["id"], 2);
}

#[test]
fn script_exit_on_nth_matching_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_script(
        &dir,
        json!([{"on":{"method":"tools/call","tool":"echo","nth":2},
            "do":[{"exit":{"code":9}}]}]),
    );
    let mut m = Mock::start(MockServerSpec::new().script(path));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.msg()["id"], 2);
    m.send(request(
        3,
        "tools/call",
        json!({"name":"echo","arguments":{}}),
    ));
    assert_eq!(m.child.wait().unwrap().code(), Some(9));
}

#[test]
fn script_string_id_and_framing_switch_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_script(
        &dir,
        json!([{"on":{"method":"ping"},
            "do":["string_id","content_length_framing","respond"]}]),
    );
    let mut m = Mock::start(MockServerSpec::new().script(path));
    m.handshake();
    m.send(request(2, "ping", json!({})));
    assert!(m.line().starts_with("Content-Length: "));
    assert_eq!(m.line(), "");
    assert_eq!(m.msg()["id"], "2");
    // The switch is sticky: later output keeps the framing and string ids.
    m.send(request(3, "ping", json!({})));
    assert!(m.line().starts_with("Content-Length: "));
    assert_eq!(m.line(), "");
    assert_eq!(m.msg()["id"], "3");
}

#[test]
fn script_oversize_without_respond_sends_only_the_oversize_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_script(
        &dir,
        json!([{"on":{"method":"tools/list"},"do":[{"oversize":{"bytes":4096}}]}]),
    );
    let mut m = Mock::start(MockServerSpec::new().script(path));
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    let line = m.line();
    assert!(line.len() >= 4096);
    assert_eq!(parse_line(&line).unwrap()["id"], 2);
    // No `respond` was listed, so the oversize line above is the only
    // answer to tools/list; the next line answers the ping.
    m.send(request(3, "ping", json!({})));
    assert_eq!(m.msg()["id"], 3);
}

#[test]
fn script_pretty_json_action_switches_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_script(
        &dir,
        json!([{"on":{"method":"ping"},"do":["pretty_json","respond"]}]),
    );
    let mut m = Mock::start(MockServerSpec::new().script(path));
    m.handshake();
    m.send(request(2, "ping", json!({})));
    assert_eq!(m.line(), "{");
    assert!(m.rx.recv_timeout(Duration::from_secs(5)).is_ok());
}

#[test]
fn big_items_returns_pretty_uniform_array() {
    let mut m = Mock::start(MockServerSpec::new().big_items(3));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"big","arguments":{}}),
    ));
    let r = m.msg();
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("[\n  {"));
    let items: Vec<Value> = serde_json::from_str(text).unwrap();
    assert_eq!(items.len(), 3);
    assert!(items
        .iter()
        .all(|i| i.get("id").is_some() && i.get("name").is_some() && i.get("active").is_some()));
    assert_eq!(items[2]["name"], "item-2");
}

#[test]
fn structured_tool_has_output_schema_and_structured_content() {
    let mut m = Mock::start(MockServerSpec::new().structured());
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    let tools = m.msg()["result"]["tools"].clone();
    let tool = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "structured")
        .unwrap();
    assert_eq!(tool["outputSchema"]["required"], json!(["tool", "value"]));
    m.send(request(
        3,
        "tools/call",
        json!({"name":"structured","arguments":{}}),
    ));
    assert_eq!(m.msg()["result"]["structuredContent"]["value"], 42);
}

#[test]
fn read_only_sets_hint_and_write_tool_is_not_read_only() {
    let mut m = Mock::start(
        MockServerSpec::new()
            .tools(&["counter", "write"])
            .read_only(&["counter"]),
    );
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    let tools = m.msg()["result"]["tools"].clone();
    let hint = |n: &str| {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .unwrap()["annotations"]["readOnlyHint"]
            .clone()
    };
    assert_eq!(hint("counter"), json!(true));
    assert_eq!(hint("write"), json!(false));
    m.send(request(
        3,
        "tools/call",
        json!({"name":"write","arguments":{"x":1}}),
    ));
    let r = m.msg();
    assert_eq!(r["result"]["structuredContent"]["tool"], "write");
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"x\":1"));
}

#[test]
fn counter_increments_per_call() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["counter"]));
    m.handshake();
    m.send(request(
        2,
        "tools/call",
        json!({"name":"counter","arguments":{}}),
    ));
    assert_eq!(m.msg()["result"]["structuredContent"]["value"], 1);
    m.send(request(
        3,
        "tools/call",
        json!({"name":"counter","arguments":{}}),
    ));
    assert_eq!(m.msg()["result"]["structuredContent"]["value"], 2);
}

#[test]
fn verbose_descriptions_are_long() {
    let mut m = Mock::start(MockServerSpec::new().tools(&["a"]).verbose_descriptions());
    m.handshake();
    m.send(request(2, "tools/list", json!({})));
    let desc = m.msg()["result"]["tools"][0]["description"]
        .as_str()
        .unwrap()
        .len();
    assert!(desc > 500);
}

/// Sends one raw HTTP POST and returns (status line, headers, body).
async fn raw_post(port: u16, body: &str) -> (String, String, String) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(s);
    let mut status = String::new();
    r.read_line(&mut status).await.unwrap();
    let mut headers = String::new();
    let mut len = 0;
    loop {
        let mut l = String::new();
        r.read_line(&mut l).await.unwrap();
        if l.trim().is_empty() {
            break;
        }
        if let Some(v) = l.strip_prefix("Content-Length:") {
            len = v.trim().parse().unwrap();
        }
        headers.push_str(&l);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.unwrap();
    (
        status.trim_end().to_owned(),
        headers,
        String::from_utf8_lossy(&buf).into_owned(),
    )
}

#[tokio::test]
async fn html_error_answers_tool_calls_with_html_502() {
    let server = MockHttpSpec::new()
        .program(HTTP_BIN)
        .html_error()
        .spawn()
        .await
        .unwrap();
    let body = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{}}})
    .to_string();
    let (status, headers, text) = raw_post(server.port(), &body).await;
    assert_eq!(status, "HTTP/1.1 502 Mock Error");
    assert!(headers
        .to_ascii_lowercase()
        .contains("content-type: text/html"));
    assert!(text.starts_with("<html>"));
}

#[tokio::test]
async fn status_answers_tool_calls_with_that_code() {
    let server = MockHttpSpec::new()
        .program(HTTP_BIN)
        .status(429)
        .spawn()
        .await
        .unwrap();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-11-25"}})
    .to_string();
    let (status, _, _) = raw_post(server.port(), &init).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{}}})
    .to_string();
    let (status, _, text) = raw_post(server.port(), &call).await;
    assert!(status.starts_with("HTTP/1.1 429"));
    assert_eq!(text, "mock error 429");
}

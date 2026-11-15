// SPDX-License-Identifier: GPL-3.0-or-later
use mcpdoze_testkit::{notification, parse_line, request, MockServerSpec};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");

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
            std::thread::sleep(Duration::from_secs(20));
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
            .recv_timeout(Duration::from_secs(15))
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

// SPDX-License-Identifier: GPL-3.0-or-later

//! MCP 2026-07-28 (stateless) lane, per the test plan of
//! docs/decisions/0002: requests and results, header validation, version
//! negotiation, discovery, removed endpoints, `subscriptions/listen`,
//! cancellation by disconnect, progress and logging on per-request SSE,
//! declined server-to-client requests, and a mixed-era run on one backend.
//! Every test has a hard timeout and shuts its broker down.

#[path = "support/harness.rs"]
mod harness;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use harness::{raw, request_text, wait_until, with_timeout, Broker, Client, Resp, Sse, MODERN};
use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::router::uri::map_uri;
use mcpdoze_core::router::{MemoryCatalog, Router};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use tokio::time::sleep;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const LIMIT: Duration = Duration::from_secs(90);
const V: &str = "io.modelcontextprotocol/protocolVersion";
const SUB_ID: &str = "io.modelcontextprotocol/subscriptionId";

async fn broker(m: MockServerSpec) -> Broker {
    let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
    let spec = BackendSpec::new(BIN, m.args())
        .idle_ttl(Duration::from_secs(30))
        .start_timeout(Duration::from_secs(20));
    router.register("mock", "m", spec).unwrap();
    // Frequent keep-alives make a vanished client show up quickly.
    router.set_sse_keepalive(Duration::from_millis(100));
    Broker::start(router).await
}

fn mock() -> MockServerSpec {
    MockServerSpec::new().tools(&["pid", "slow", "echo", "elicit", "touch"])
}

/// A modern request: `params._meta` carries the version, capabilities and
/// client info.
fn mreq(id: Value, method: &str, params: Value) -> Value {
    let mut p = if params.is_object() {
        params
    } else {
        json!({})
    };
    let meta = p
        .as_object_mut()
        .unwrap()
        .entry("_meta")
        .or_insert_with(|| json!({}));
    meta[V] = json!(MODERN);
    meta["io.modelcontextprotocol/clientCapabilities"] = json!({});
    meta["io.modelcontextprotocol/clientInfo"] = json!({"name":"test","version":"0"});
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":p})
}

/// The headers a conforming client mirrors from `msg`.
fn mheaders(msg: &Value) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "MCP-Protocol-Version".to_owned(),
            msg["params"]["_meta"][V]
                .as_str()
                .unwrap_or(MODERN)
                .to_owned(),
        ),
        (
            "Mcp-Method".to_owned(),
            msg["method"].as_str().unwrap().to_owned(),
        ),
    ];
    let name = match msg["method"].as_str().unwrap() {
        "tools/call" | "prompts/get" => msg["params"]["name"].as_str(),
        "resources/read" => msg["params"]["uri"].as_str(),
        _ => None,
    };
    if let Some(n) = name {
        h.push(("Mcp-Name".to_owned(), n.to_owned()));
    }
    h
}

async fn post_with(addr: SocketAddr, msg: &Value, headers: &[(String, String)]) -> Resp {
    let h: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    raw(addr, "POST", "/mcp", &h, &msg.to_string()).await
}

async fn mpost(addr: SocketAddr, msg: &Value) -> Resp {
    post_with(addr, msg, &mheaders(msg)).await
}

/// Opens a streamed modern POST.
async fn mstream(addr: SocketAddr, msg: &Value) -> Sse {
    let headers = mheaders(msg);
    let h: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    Sse::open(
        addr,
        request_text("POST", "/mcp", &h, &msg.to_string(), true),
    )
    .await
}

fn call(id: i64, tool: &str, args: Value) -> Value {
    mreq(
        json!(id),
        "tools/call",
        json!({"name": format!("m__{tool}"), "arguments": args}),
    )
}

fn without(h: &[(String, String)], name: &str) -> Vec<(String, String)> {
    h.iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(name))
        .cloned()
        .collect()
}

fn with(h: &[(String, String)], name: &str, value: &str) -> Vec<(String, String)> {
    let mut out = without(h, name);
    out.push((name.to_owned(), value.to_owned()));
    out
}

fn b64(s: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in s.as_bytes().chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn assert_code(r: &Resp, status: u16, code: i64) {
    assert_eq!(r.status, status, "{r:?}");
    assert_eq!(r.json()["error"]["code"], code, "{r:?}");
}

/// 1. tools/list and tools/call: 200, resultType, caching fields on lists,
/// serverInfo, no session header; a stray Mcp-Session-Id is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_and_results() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let list = mreq(json!(1), "tools/list", json!({}));
        let mut h = mheaders(&list);
        h.push(("Mcp-Session-Id".into(), "stray-session".into()));
        let r = post_with(b.addr, &list, &h).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert!(r.header("mcp-session-id").is_none());
        let v = r.json();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["resultType"], "complete");
        assert_eq!(v["result"]["cacheScope"], "private");
        assert!(v["result"]["ttlMs"].as_u64().is_some());
        assert_eq!(
            v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "mcpdoze"
        );
        assert!(v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "m__echo"));

        let r = mpost(b.addr, &call(2, "echo", json!({"x": 1}))).await;
        assert_eq!(r.status, 200, "{r:?}");
        let v = r.json();
        assert_eq!(v["result"]["resultType"], "complete");
        assert!(v["result"].get("ttlMs").is_none());
        // The backend never saw the modern _meta keys.
        assert_eq!(
            v["result"]["content"][0]["text"],
            json!({"x": 1}).to_string()
        );
        assert_eq!(b.router.session_count(), 0);
        assert_eq!(b.router.live_session_count(), 0);
        b.finish().await;
    })
    .await;
}

/// 2. Header validation: missing or mismatched headers get 400 -32020; a
/// Base64-sentinel Mcp-Name is decoded; a modern header without _meta too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_validation() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let msg = call(1, "echo", json!({}));
        let h = mheaders(&msg);
        for bad in [
            without(&h, "MCP-Protocol-Version"),
            with(&h, "MCP-Protocol-Version", "2025-11-25"),
            without(&h, "Mcp-Method"),
            with(&h, "Mcp-Method", "tools/list"),
            without(&h, "Mcp-Name"),
            with(&h, "Mcp-Name", "m__other"),
            with(&h, "Mcp-Name", "=?base64?!!!?="),
        ] {
            let r = post_with(b.addr, &msg, &bad).await;
            assert_code(&r, 400, -32020);
            assert_eq!(r.json()["id"], 1);
        }
        let ok = with(&h, "Mcp-Name", &format!("=?base64?{}?=", b64("m__echo")));
        assert_eq!(post_with(b.addr, &msg, &ok).await.status, 200);
        // resources/read mirrors the URI.
        let uri = map_uri("m", "mock://readme");
        let read = mreq(json!(2), "resources/read", json!({"uri": uri}));
        let r = mpost(b.addr, &read).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.json()["result"]["cacheScope"], "private");
        let r = post_with(
            b.addr,
            &read,
            &with(&mheaders(&read), "Mcp-Name", "mcpdoze://m/x"),
        )
        .await;
        assert_code(&r, 400, -32020);
        // Modern header, no _meta.
        let bare = json!({"jsonrpc":"2.0","id":3,"method":"tools/list"});
        let r = post_with(
            b.addr,
            &bare,
            &mheaders(&mreq(json!(3), "tools/list", json!({}))),
        )
        .await;
        assert_code(&r, 400, -32020);
        b.finish().await;
    })
    .await;
}

/// 3. An unsupported version: 400 -32022 with the supported list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_version() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let mut msg = mreq(json!(1), "tools/list", json!({}));
        msg["params"]["_meta"][V] = json!("1900-01-01");
        let r = mpost(b.addr, &msg).await;
        assert_code(&r, 400, -32022);
        let data = &r.json()["error"]["data"];
        assert_eq!(data["requested"], "1900-01-01");
        let sup = data["supported"].as_array().unwrap();
        assert!(
            sup.contains(&json!(MODERN)) && sup.contains(&json!("2025-11-25")),
            "{data}"
        );
        b.finish().await;
    })
    .await;
}

/// 4. server/discover answers from the broker's own capabilities.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let r = mpost(b.addr, &mreq(json!("d"), "server/discover", json!({}))).await;
        assert_eq!(r.status, 200, "{r:?}");
        let v = r.json()["result"].clone();
        assert_eq!(v["supportedVersions"][0], MODERN);
        assert_eq!(v["resultType"], "complete");
        assert_eq!(v["cacheScope"], "private");
        assert!(v["ttlMs"].as_u64().is_some());
        let init = Client::init_result(b.addr).await;
        assert_eq!(v["capabilities"], init["capabilities"]);
        // Nothing was spawned to answer it.
        assert_eq!(b.router.instance("mock").unwrap().spawn_count(), 0);
        b.finish().await;
    })
    .await;
}

/// 5. GET and DELETE on the modern lane: 405 (stray session ids ignored);
/// unknown and removed methods: 404 -32601.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_endpoints_and_methods() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let legacy = Client::init(b.addr).await;
        for m in ["GET", "DELETE"] {
            let r = raw(
                b.addr,
                m,
                "/mcp",
                &[
                    ("MCP-Protocol-Version", MODERN),
                    ("Mcp-Session-Id", &legacy.session),
                    ("Last-Event-ID", "7"),
                    ("Accept", "text/event-stream"),
                ],
                "",
            )
            .await;
            assert_eq!(r.status, 405, "{m}: {r:?}");
        }
        // The legacy session was not touched by the modern DELETE.
        assert_eq!(b.router.session_count(), 1);
        let r = raw(b.addr, "DELETE", "/mcp", &[], "").await;
        assert_eq!(r.status, 405, "{r:?}");
        for m in [
            "nope/nope",
            "ping",
            "initialize",
            "resources/subscribe",
            "logging/setLevel",
        ] {
            let r = mpost(b.addr, &mreq(json!(1), m, json!({}))).await;
            assert_code(&r, 404, -32601);
        }
        // A modern notification is accepted and ignored.
        let mut n = mreq(
            json!(null),
            "notifications/cancelled",
            json!({"requestId": 1}),
        );
        n.as_object_mut().unwrap().remove("id");
        assert_eq!(mpost(b.addr, &n).await.status, 202);
        b.finish().await;
    })
    .await;
}

/// 6. subscriptions/listen: acknowledgment first, opted-in list changes
/// only, tagged; resource subscriptions through the registry; graceful
/// close on shutdown; a dropped stream releases its subscriptions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriptions_listen() {
    with_timeout(LIMIT, async {
        let b = broker(mock().subscribe().list_changed_after(1)).await;
        assert_eq!(mpost(b.addr, &mreq(json!(1), "resources/list", json!({}))).await.status, 200);
        let uri = map_uri("m", "mock://readme");
        let listen = mreq(
            json!("L1"),
            "subscriptions/listen",
            json!({"notifications": {"toolsListChanged": true, "resourceSubscriptions": [uri, "mcpdoze://zz/x"]}}),
        );
        let mut s = mstream(b.addr, &listen).await;
        let ack = s.next("ack", |m| m.get("method").is_some()).await;
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged", "{ack}");
        assert_eq!(ack["params"]["_meta"][SUB_ID], "L1");
        assert_eq!(ack["params"]["notifications"], json!({"toolsListChanged": true, "resourceSubscriptions": [uri]}));
        assert_eq!(b.router.subscription_stats().active, 1);

        // A call makes the mock announce a tools change.
        assert_eq!(mpost(b.addr, &call(2, "echo", json!({}))).await.status, 200);
        let n = s.next_method("notifications/tools/list_changed").await;
        assert_eq!(n["params"]["_meta"][SUB_ID], "L1");
        // A resource update reaches the listener, mapped and tagged.
        assert_eq!(mpost(b.addr, &call(3, "touch", json!({}))).await.status, 200);
        let u = s.next_method("notifications/resources/updated").await;
        assert_eq!(u["params"]["uri"], uri);
        assert_eq!(u["params"]["_meta"][SUB_ID], "L1");
        assert!(s.messages().iter().all(|m| m["method"] != "notifications/progress"));

        // A second listener without filters gets nothing but its ack; a
        // broker-side close ends it gracefully.
        let mut quiet = mstream(b.addr, &mreq(json!(9), "subscriptions/listen", json!({"notifications": {}}))).await;
        quiet.next_method("notifications/subscriptions/acknowledged").await;
        b.router.close_streams();
        let done = quiet.next("final", |m| m.get("result").is_some()).await;
        assert_eq!(done["id"], 9);
        assert_eq!(done["result"]["resultType"], "complete");
        assert_eq!(done["result"]["_meta"][SUB_ID], 9);
        quiet.read_to_close().await;
        assert_eq!(quiet.messages().len(), 2, "{}", quiet.text());
        drop(s);
        let r = b.router.clone();
        wait_until("listen release", Duration::from_secs(10), || {
            r.subscription_stats().active == 0 && r.live_session_count() == 0
        })
        .await;
        b.finish().await;
    })
    .await;
}

/// 6b. A client closing its listen stream releases the subscription.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_drop_releases() {
    with_timeout(LIMIT, async {
        let b = broker(mock().subscribe()).await;
        mpost(b.addr, &mreq(json!(1), "resources/list", json!({}))).await;
        let uri = map_uri("m", "mock://readme");
        let listen = mreq(
            json!(5),
            "subscriptions/listen",
            json!({"notifications": {"resourceSubscriptions": [uri]}}),
        );
        let mut s = mstream(b.addr, &listen).await;
        s.next_method("notifications/subscriptions/acknowledged")
            .await;
        assert_eq!(b.router.subscription_stats().active, 1);
        drop(s);
        let r = b.router.clone();
        wait_until("release on drop", Duration::from_secs(10), || {
            let st = r.subscription_stats();
            st.active == 0 && st.backend_unsubscribes == 1 && r.live_session_count() == 0
        })
        .await;
        b.finish().await;
    })
    .await;
}

/// 7. Disconnecting mid-call cancels the backend call; no session leaks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_cancels() {
    with_timeout(LIMIT, async {
        let b = broker(mock().delay_ms(10_000)).await;
        let mut msg = call(1, "slow", json!({}));
        msg["params"]["_meta"]["progressToken"] = json!("p-1");
        let mut s = mstream(b.addr, &msg).await;
        s.next_method("notifications/progress").await;
        let inst = b.router.instance("mock").unwrap();
        assert_eq!(inst.in_flight(), 1);
        assert_eq!(b.router.live_session_count(), 1);
        drop(s);
        let r = b.router.clone();
        wait_until("cancel", Duration::from_secs(5), || {
            inst.in_flight() == 0 && inst.pending_calls() == 0 && r.live_session_count() == 0
        })
        .await;
        // The backend is still healthy.
        assert_eq!(mpost(b.addr, &call(2, "echo", json!({}))).await.status, 200);
        b.finish().await;
    })
    .await;
}

/// 8. Progress and logging on the request's SSE response, against a
/// 2025-11-25 stdio backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_and_logging_stream() {
    with_timeout(LIMIT, async {
        let b = broker(mock().delay_ms(200).log_messages()).await;
        let mut msg = call(1, "slow", json!({"a": 1}));
        msg["params"]["_meta"]["progressToken"] = json!("tok-1");
        msg["params"]["_meta"]["io.modelcontextprotocol/logLevel"] = json!("info");
        let mut s = mstream(b.addr, &msg).await;
        let p = s.next_method("notifications/progress").await;
        assert_eq!(p["params"]["progressToken"], "tok-1");
        let done = s.next("result", |m| m.get("result").is_some()).await;
        assert_eq!(done["id"], 1);
        assert_eq!(done["result"]["resultType"], "complete");
        s.read_to_close().await;
        let all = s.messages();
        assert!(
            all.iter().any(|m| m["method"] == "notifications/message"),
            "{}",
            s.text()
        );
        assert_eq!(
            all.iter()
                .filter(|m| m["method"] == "notifications/progress")
                .count(),
            2
        );
        assert!(s.text().contains("text/event-stream"));
        // A higher threshold filters the info message out.
        let mut msg = call(2, "slow", json!({}));
        msg["params"]["_meta"]["io.modelcontextprotocol/logLevel"] = json!("error");
        let mut s = mstream(b.addr, &msg).await;
        s.next("result", |m| m.get("result").is_some()).await;
        assert!(s
            .messages()
            .iter()
            .all(|m| m["method"] != "notifications/message"));
        b.finish().await;
    })
    .await;
}

/// 9. A backend elicitation during a modern call is declined at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_is_declined() {
    with_timeout(LIMIT, async {
        let b = broker(mock().elicitation_request()).await;
        let started = tokio::time::Instant::now();
        let r = mpost(b.addr, &call(1, "elicit", json!({}))).await;
        assert_eq!(r.status, 200, "{r:?}");
        let text = r.json()["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(text.contains("declined"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(10));
        b.finish().await;
    })
    .await;
}

/// 10. Mixed eras on one backend: a legacy session (with its GET stream)
/// and modern requests share one instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_eras_share_one_instance() {
    with_timeout(LIMIT, async {
        let b = broker(mock()).await;
        let legacy = Client::init(b.addr).await;
        let mut stream = legacy.sse().await;
        let lp = harness::structured(&legacy.call(1, "pid", json!({})).await)["pid"].clone();
        let r = mpost(b.addr, &call(2, "pid", json!({}))).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.json()["result"]["structuredContent"]["pid"], lp);
        let lr = legacy.rpc(3, "tools/list", json!({})).await;
        assert!(lr["result"].get("resultType").is_none(), "{lr}");
        assert_eq!(b.router.instance("mock").unwrap().spawn_count(), 1);
        // The legacy stream is still served.
        b.router
            .broadcast(&json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
        stream.next_method("notifications/tools/list_changed").await;
        assert_eq!(legacy.delete().await, 200);
        sleep(Duration::from_millis(50)).await;
        b.finish().await;
    })
    .await;
}

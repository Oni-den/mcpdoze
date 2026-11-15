// SPDX-License-Identifier: GPL-3.0-or-later

//! Resource subscriptions (docs/SUBSCRIPTIONS.md): one backend subscription
//! per (server, instance, URI) whatever the number of agent sessions,
//! release on unsubscribe and session end, the idle policies, replay after a
//! crash, per-cwd isolation, the subscribe-versus-idle-unload race and the
//! HTTP upstream event stream. Every test has a hard timeout and shuts its
//! broker down.

#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{kill, structured, wait_until, with_timeout, Broker, Client};
use mcpdoze_core::backend::{BackendSpec, StateKind, SubscriptionPolicy};
use mcpdoze_core::config::Scope;
use mcpdoze_core::router::uri::map_uri;
use mcpdoze_core::router::{CatalogOptions, MemoryCatalog, Router, ScopeOptions};
use mcpdoze_testkit::{MockHttpSpec, MockServerSpec};
use serde_json::{json, Value};
use tokio::time::sleep;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const HTTP_BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-http");
const LIMIT: Duration = Duration::from_secs(90);
const URI: &str = "mock://readme";

fn mock() -> MockServerSpec {
    MockServerSpec::new()
        .tools(&["pid", "touch", "sub_stats"])
        .subscribe()
}

fn spec(m: MockServerSpec, ttl: Duration, policy: SubscriptionPolicy) -> BackendSpec {
    let mut s = BackendSpec::new(BIN, m.args())
        .idle_ttl(ttl)
        .start_timeout(Duration::from_secs(20));
    s.subscriptions = policy;
    s
}

async fn broker(s: BackendSpec, scope: Scope) -> Broker {
    let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
    router
        .register_scoped(
            "mock",
            "m",
            s,
            CatalogOptions::default(),
            ScopeOptions {
                scope,
                max_instances: 4,
            },
        )
        .unwrap();
    Broker::start(router).await
}

fn mapped() -> String {
    map_uri("m", URI)
}

async fn subscribe(c: &Client, id: i64) -> Value {
    c.rpc(id, "resources/subscribe", json!({"uri": mapped()}))
        .await
}

async fn unsubscribe(c: &Client, id: i64) -> Value {
    c.rpc(id, "resources/unsubscribe", json!({"uri": mapped()}))
        .await
}

async fn stats(c: &Client, id: i64) -> Value {
    structured(&c.call(id, "sub_stats", json!({})).await)
}

fn ok(resp: &Value) {
    assert!(resp.get("result").is_some(), "{resp}");
}

/// Two sessions, one URI: one backend subscribe; updates reach both, with
/// the mapped URI; the last unsubscribe sends one backend unsubscribe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_share_one_backend_subscription() {
    with_timeout(LIMIT, async {
        let b = broker(
            spec(mock(), Duration::from_secs(30), Default::default()),
            Scope::Shared,
        )
        .await;
        let (c1, c2) = tokio::join!(Client::init(b.addr), Client::init(b.addr));
        // Listing learns the capability; initialize then advertises it.
        let list = c1.rpc(1, "resources/list", json!({})).await;
        assert_eq!(list["result"]["resources"][0]["uri"], mapped(), "{list}");
        let caps = Client::init_result(b.addr).await;
        assert_eq!(
            caps["capabilities"]["resources"]["subscribe"], true,
            "{caps}"
        );
        let (mut s1, mut s2) = (c1.sse().await, c2.sse().await);
        ok(&subscribe(&c1, 2).await);
        ok(&subscribe(&c2, 2).await);
        ok(&subscribe(&c2, 3).await); // repeated: no second backend call
        let st = stats(&c1, 4).await;
        assert_eq!(st["subscribes"], 1, "{st}");
        assert_eq!(st["active"], json!([URI]), "{st}");

        c1.call(5, "touch", json!({"uri": URI})).await;
        for s in [&mut s1, &mut s2] {
            let n = s.next_method("notifications/resources/updated").await;
            assert_eq!(n["params"]["uri"], mapped(), "{n}");
        }

        ok(&unsubscribe(&c1, 6).await);
        assert_eq!(stats(&c1, 7).await["unsubscribes"], 0);
        ok(&unsubscribe(&c2, 8).await);
        let st = stats(&c1, 9).await;
        assert_eq!(st["unsubscribes"], 1, "{st}");
        assert_eq!(st["active"], json!([]), "{st}");
        let rs = b.router.subscription_stats();
        assert_eq!(
            (rs.active, rs.backend_subscribes, rs.backend_unsubscribes),
            (0, 1, 1)
        );
        assert_eq!(b.router.instance("mock").unwrap().pins(), 0);
        b.finish().await;
    })
    .await;
}

/// Ending a session (DELETE) releases its subscriptions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_releases_subscriptions() {
    with_timeout(LIMIT, async {
        let b = broker(
            spec(mock(), Duration::from_secs(30), Default::default()),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        ok(&subscribe(&c, 2).await);
        assert_eq!(b.router.subscription_stats().active, 1);
        assert_eq!(c.delete().await, 200);
        let r = b.router.clone();
        wait_until("release", Duration::from_secs(10), || {
            let s = r.subscription_stats();
            s.active == 0 && s.backend_unsubscribes == 1
        })
        .await;
        let other = Client::init(b.addr).await;
        assert_eq!(stats(&other, 1).await["active"], json!([]));
        b.finish().await;
    })
    .await;
}

/// keep-alive: a pinned instance outlives its idle TTL. allow-unload: it
/// unloads, and the next call's new process gets the subscription back plus
/// a synthetic update.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_policies() {
    with_timeout(LIMIT, async {
        let ttl = Duration::from_millis(400);
        // keep-alive
        let b = broker(
            spec(mock(), ttl, SubscriptionPolicy::KeepAlive),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        ok(&subscribe(&c, 2).await);
        let inst = b.router.instance("mock").unwrap();
        let pid = inst.pid().unwrap();
        sleep(ttl * 4).await;
        assert_eq!(inst.state(), StateKind::Running);
        assert_eq!(inst.pid(), Some(pid));
        ok(&unsubscribe(&c, 3).await);
        wait_until("unload after unpin", Duration::from_secs(10), || {
            inst.state() == StateKind::Stopped
        })
        .await;
        b.finish().await;

        // allow-unload
        let b = broker(
            spec(mock(), ttl, SubscriptionPolicy::AllowUnload),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        let mut sse = c.sse().await;
        ok(&subscribe(&c, 2).await);
        let inst = b.router.instance("mock").unwrap();
        wait_until("idle unload", Duration::from_secs(10), || {
            inst.state() == StateKind::Stopped
        })
        .await;
        assert_eq!(b.router.subscription_stats().active, 1);
        // The next call starts a new process; the subscription is replayed.
        let r = b.router.clone();
        c.call(3, "pid", json!({})).await;
        wait_until("replay", Duration::from_secs(10), || {
            r.subscription_stats().backend_subscribes == 2
        })
        .await;
        let n = sse.next_method("notifications/resources/updated").await;
        assert_eq!(n["params"]["uri"], mapped());
        let st = stats(&c, 4).await;
        assert_eq!(
            (st["subscribes"].clone(), st["active"].clone()),
            (json!(1), json!([URI]))
        );
        b.finish().await;
    })
    .await;
}

/// keep-alive: a crashed pinned backend is respawned without any call and
/// resubscribed; subscribers get a synthetic update.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_respawns_and_resubscribes() {
    with_timeout(LIMIT, async {
        let b = broker(
            spec(
                mock(),
                Duration::from_secs(30),
                SubscriptionPolicy::KeepAlive,
            ),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        let mut sse = c.sse().await;
        ok(&subscribe(&c, 2).await);
        let inst = b.router.instance("mock").unwrap();
        let old = inst.pid().unwrap();
        kill(old);
        wait_until("eager respawn", Duration::from_secs(15), || {
            inst.pid().is_some_and(|p| p != old)
        })
        .await;
        let n = sse.next_method("notifications/resources/updated").await;
        assert_eq!(n["params"]["uri"], mapped());
        assert_eq!(b.router.subscription_stats().backend_subscribes, 2);
        let st = stats(&c, 3).await;
        assert_eq!(st["active"], json!([URI]), "{st}");
        assert_ne!(st["pid"], json!(old));
        b.finish().await;
    })
    .await;
}

/// per-cwd: the same URI in two directories means two instances, two
/// backend subscriptions, and updates stay in their directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_cwd_subscriptions_are_isolated() {
    with_timeout(LIMIT, async {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (pa, pb) = (
            da.path().to_string_lossy().into_owned(),
            db.path().to_string_lossy().into_owned(),
        );
        let b = broker(
            spec(mock(), Duration::from_secs(30), Default::default()),
            Scope::PerCwd,
        )
        .await;
        let ca = Client::init_with(b.addr, &[("X-Mcpdoze-Cwd", &pa)]).await;
        let cb = Client::init_with(b.addr, &[("X-Mcpdoze-Cwd", &pb)]).await;
        ca.rpc(1, "resources/list", json!({})).await;
        let (mut sa, mut sb) = (ca.sse().await, cb.sse().await);
        ok(&subscribe(&ca, 2).await);
        ok(&subscribe(&cb, 2).await);
        let rs = b.router.subscription_stats();
        assert_eq!((rs.active, rs.backend_subscribes), (2, 2));
        let (pid_a, pid_b) = (
            stats(&ca, 3).await["pid"].clone(),
            stats(&cb, 3).await["pid"].clone(),
        );
        assert_ne!(pid_a, pid_b);

        ca.call(4, "touch", json!({"uri": URI})).await;
        sa.next_method("notifications/resources/updated").await;
        cb.call(4, "touch", json!({"uri": URI})).await;
        sb.next_method("notifications/resources/updated").await;
        sleep(Duration::from_millis(200)).await;
        let count = |s: &harness::Sse| {
            s.messages()
                .iter()
                .filter(|m| m["method"] == "notifications/resources/updated")
                .count()
        };
        assert_eq!(count(&sa), 1);
        assert_eq!(count(&sb), 1);
        b.finish().await;
    })
    .await;
}

/// A subscribe landing right at the idle deadline either finds the process
/// running (and pins it) or starts a new one; it never ends up subscribed
/// on a process that is being unloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_races_idle_unload() {
    with_timeout(LIMIT, async {
        let ttl = Duration::from_millis(150);
        let b = broker(
            spec(mock(), ttl, SubscriptionPolicy::KeepAlive),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        let inst = b.router.instance("mock").unwrap();
        for i in 0..8u64 {
            c.call(10 + i as i64, "pid", json!({})).await;
            // Land around the idle deadline.
            sleep(ttl - Duration::from_millis(40) + Duration::from_millis(i * 10)).await;
            ok(&subscribe(&c, 100 + i as i64).await);
            assert_eq!(inst.pins(), 1);
            sleep(ttl * 2).await;
            assert_eq!(inst.state(), StateKind::Running, "round {i}");
            let st = stats(&c, 200 + i as i64).await;
            assert_eq!(st["active"], json!([URI]), "round {i}: {st}");
            ok(&unsubscribe(&c, 300 + i as i64).await);
            assert_eq!(inst.pins(), 0);
        }
        b.finish().await;
    })
    .await;
}

/// A backend without `resources.subscribe`: the broker does not advertise
/// it and answers `-32601`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_backend_is_refused() {
    with_timeout(LIMIT, async {
        let b = broker(
            spec(
                MockServerSpec::new().tools(&["pid"]),
                Duration::from_secs(30),
                Default::default(),
            ),
            Scope::Shared,
        )
        .await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "resources/list", json!({})).await;
        let r = subscribe(&c, 2).await;
        assert_eq!(r["error"]["code"], -32601, "{r}");
        let caps = Client::init_result(b.addr).await;
        assert_eq!(caps["capabilities"]["resources"]["subscribe"], false);
        b.finish().await;
    })
    .await;
}

/// HTTP backend: subscribing opens the upstream GET stream; upstream
/// updates reach the subscriber; the last unsubscribe closes the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_backend_listens_while_pinned() {
    with_timeout(LIMIT, async {
        let remote = MockHttpSpec::new()
            .program(HTTP_BIN)
            .subscribe()
            .spawn()
            .await
            .unwrap();
        let s = BackendSpec::http(remote.url())
            .idle_ttl(Duration::from_secs(30))
            .start_timeout(Duration::from_secs(20));
        let b = broker(s, Scope::Shared).await;
        let c = Client::init(b.addr).await;
        let list = c.rpc(1, "resources/list", json!({})).await;
        let uri = map_uri("m", "mock://remote");
        assert_eq!(list["result"]["resources"][0]["uri"], uri, "{list}");
        let mut sse = c.sse().await;
        ok(&c.rpc(2, "resources/subscribe", json!({"uri": uri})).await);
        let text = |v: Value| -> Value {
            serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let mut n = 10;
        loop {
            n += 1;
            let st = text(c.call(n, "sub_stats", json!({})).await);
            if st["listeners"] == 1 {
                assert_eq!(st["subscribes"], 1);
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        c.call(50, "touch", json!({"uri": "mock://remote"})).await;
        let u = sse.next_method("notifications/resources/updated").await;
        assert_eq!(u["params"]["uri"], uri);
        ok(&c
            .rpc(51, "resources/unsubscribe", json!({"uri": uri}))
            .await);
        let mut n = 60;
        loop {
            n += 1;
            let st = text(c.call(n, "sub_stats", json!({})).await);
            if st["listeners"] == 0 {
                assert_eq!(st["unsubscribes"], 1);
                break;
            }
            assert!(n < 200, "upstream stream not closed: {st}");
            sleep(Duration::from_millis(50)).await;
        }
        b.finish().await;
        drop(remote);
    })
    .await;
}

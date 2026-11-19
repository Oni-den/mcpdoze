// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase A1 acceptance: a denied tool is absent from `tools/list` with zero
//! backends running and a call gives -32602; `list_bytes` drops; a config
//! change emits `list_changed` without a respawn (same pid); a `default_args`
//! value reaches the mock only when the agent omitted it; `minify_json`
//! results are valid and smaller.

#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{with_timeout, Broker, Client};
use mcpdoze_core::admin::Control;
use mcpdoze_core::backend::{BackendSpec, StateKind};
use mcpdoze_core::config::Config;
use mcpdoze_core::router::optimize::ServerPolicy;
use mcpdoze_core::router::{MemoryCatalog, Router};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const LIMIT: Duration = Duration::from_secs(90);

fn spec(mock: MockServerSpec) -> BackendSpec {
    BackendSpec::new(BIN, mock.args()).start_timeout(Duration::from_secs(20))
}

/// The compiled policy of server `m` with `extra` appended to its table.
fn policy(extra: &str) -> ServerPolicy {
    let cfg = Config::parse(&format!(
        "[servers.m]\ntransport = \"stdio\"\ncommand = \"x\"\n{extra}"
    ))
    .unwrap();
    cfg.validate().unwrap();
    ServerPolicy::compile(&cfg.servers["m"]).unwrap().unwrap()
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tool_is_hidden_and_refused() {
    with_timeout(LIMIT, async {
        let catalog = Arc::new(MemoryCatalog::default());
        let mock = || {
            MockServerSpec::new()
                .tools(&["echo", "secret"])
                .verbose_descriptions()
        };

        // Fill the catalog with an unmodified router.
        let r1 = Router::with(catalog.clone(), harness::tuning());
        r1.register("m", "m", spec(mock())).unwrap();
        let b1 = Broker::start(r1.clone()).await;
        let c = Client::init(b1.addr).await;
        let names = tool_names(&c.rpc(1, "tools/list", json!({})).await);
        assert!(names.contains(&"m__secret".to_owned()), "{names:?}");
        let before = r1.tool_list_bytes()["m"];
        b1.finish().await;

        // A second router over the same catalog: nothing is running.
        let r2 = Router::with(catalog, harness::tuning());
        r2.register("m", "m", spec(mock())).unwrap();
        r2.set_policy(
            "m",
            Some(policy(
                "[servers.m.optimize]\ntools_deny = [\"secret\"]\ndescription_max_chars = 20\n",
            )),
        );
        let b2 = Broker::start(r2.clone()).await;
        let c = Client::init(b2.addr).await;
        let list = c.rpc(1, "tools/list", json!({})).await;
        assert_eq!(tool_names(&list), vec!["m__echo".to_owned()], "{list}");
        let desc = list["result"]["tools"][0]["description"].as_str().unwrap();
        assert!(desc.chars().count() <= 20, "{desc}");
        assert_ne!(
            r2.server("m").unwrap().instance.state(),
            StateKind::Running,
            "listing must not start the backend"
        );
        assert!(r2.tool_list_bytes()["m"] < before);

        let r = c.call(2, "secret", json!({})).await;
        assert_eq!(r["error"]["code"], -32602, "{r}");
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
        let r = c.call(3, "echo", json!({"a": 1})).await;
        assert!(r["result"].is_object(), "{r}");
        b2.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_args_only_fill_omitted_keys() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register("m", "m", spec(MockServerSpec::new().tools(&["echo"])))
            .unwrap();
        router.set_policy(
            "m",
            Some(policy(
                "[servers.m.tools.echo]\ndefault_args = { per_page = 20 }\n",
            )),
        );
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        let seen = |r: &Value| -> Value {
            serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let r = c.call(1, "echo", json!({"q": "x"})).await;
        assert_eq!(seen(&r), json!({"q": "x", "per_page": 20}), "{r}");
        let r = c.call(2, "echo", json!({"q": "x", "per_page": 5})).await;
        assert_eq!(seen(&r), json!({"q": "x", "per_page": 5}), "{r}");
        let r = c.call(3, "echo", json!({})).await;
        assert_eq!(seen(&r), json!({"per_page": 20}), "{r}");
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minify_json_is_smaller_and_lossless() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(MockServerSpec::new().tools(&["echo"]).big_items(40)),
            )
            .unwrap();
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        let raw = c.call(1, "big", json!({})).await;
        let raw_text = raw["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();

        router.set_policy(
            "m",
            Some(policy("[servers.m.optimize]\nminify_json = true\n")),
        );
        let min = c.call(2, "big", json!({})).await;
        let min_text = min["result"]["content"][0]["text"].as_str().unwrap();
        assert!(min_text.len() < raw_text.len());
        assert!(!min_text.contains('\n'));
        let a: Value = serde_json::from_str(&raw_text).unwrap();
        let b2: Value = serde_json::from_str(min_text).unwrap();
        assert_eq!(a, b2);
        assert_eq!(
            min["result"]["structuredContent"],
            raw["result"]["structuredContent"]
        );
        // A plain (non-JSON-array) text result passes through unchanged.
        let e = c.call(3, "echo", json!({})).await;
        assert_eq!(e["result"]["content"][0]["text"], "{}");
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_change_notifies_without_respawn() {
    with_timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mock = MockServerSpec::new().tools(&["echo", "info"]);
        let args: Vec<String> = mock.args().iter().map(|a| format!("'{a}'")).collect();
        let base = format!(
            "[servers.m]\ntransport = \"stdio\"\ncommand = '{BIN}'\nargs = [{}]\n",
            args.join(", ")
        );
        std::fs::write(&path, &base).unwrap();

        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        let control = Control::new(
            router.clone(),
            Config::load(&path).unwrap(),
            Some(path.clone()),
        );
        control.apply_initial().await;
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        let mut sse = c.sse().await;
        let pid = |r: &Value| r["result"]["structuredContent"]["pid"].as_u64().unwrap();
        let before = pid(&c.call(1, "info", json!({})).await);
        assert_eq!(
            tool_names(&c.rpc(2, "tools/list", json!({})).await).len(),
            2
        );

        std::fs::write(
            &path,
            format!("{base}[servers.m.optimize]\ntools_deny = [\"info\"]\n"),
        )
        .unwrap();
        let resp = control.reload().await.unwrap();
        assert!(resp.optimize_changed && resp.changed.is_empty(), "{resp:?}");
        sse.next_method("notifications/tools/list_changed").await;
        assert_eq!(
            tool_names(&c.rpc(3, "tools/list", json!({})).await),
            vec!["m__echo".to_owned()]
        );
        // Same backend process: the denied tool is refused, echo still works
        // and a reload that removes the table brings `info` back with the
        // same pid.
        let r = c.call(4, "info", json!({})).await;
        assert_eq!(r["error"]["code"], -32602, "{r}");
        std::fs::write(&path, &base).unwrap();
        control.reload().await.unwrap();
        let after = pid(&c.call(5, "info", json!({})).await);
        assert_eq!(before, after, "backend must not be respawned");
        b.finish().await;
    })
    .await;
}

// ---------------------------------------------------------------- phase A2

async fn result_get(c: &Client, id: i64, args: Value) -> Value {
    c.rpc(
        id,
        "tools/call",
        json!({"name":"mcpdoze__result_get","arguments":args}),
    )
    .await
}

fn last_text(r: &Value) -> String {
    let blocks = r["result"]["content"].as_array().unwrap();
    blocks.last().unwrap()["text"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_result_pages_reassemble_and_refs_are_session_bound() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(MockServerSpec::new().tools(&["echo"]).big_items(300)),
            )
            .unwrap();
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        let raw = c.call(1, "big", json!({})).await;
        let raw_text = raw["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(raw_text.len() > 5000);
        // Not listed without the feature.
        assert!(!tool_names(&c.rpc(2, "tools/list", json!({})).await)
            .contains(&"mcpdoze__result_get".to_owned()));

        router.set_policy(
            "m",
            Some(policy("[servers.m.optimize]\nmax_result_bytes = 1024\n")),
        );
        assert!(tool_names(&c.rpc(3, "tools/list", json!({})).await)
            .contains(&"mcpdoze__result_get".to_owned()));
        let t = c.call(4, "big", json!({})).await;
        let blocks = t["result"]["content"].as_array().unwrap();
        let shown = blocks[0]["text"].as_str().unwrap();
        assert!(
            shown.len() < 1400 && shown.len() < raw_text.len(),
            "{}",
            shown.len()
        );
        let marker = last_text(&t);
        assert!(marker.starts_with("[mcpdoze: truncated,"), "{marker}");
        let id = marker
            .split("ref=")
            .nth(1)
            .unwrap()
            .trim_end_matches(']')
            .to_owned();

        // Page through with an awkward limit and reassemble.
        let mut got = String::new();
        let mut offset = 0u64;
        for n in 0..200 {
            let r = result_get(&c, 10 + n, json!({"ref":id,"offset":offset,"limit":777})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            got.push_str(r["result"]["content"][0]["text"].as_str().unwrap());
            let sc = &r["result"]["structuredContent"];
            assert_eq!(sc["total"].as_u64().unwrap() as usize, raw_text.len());
            match sc["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(got, raw_text, "pages must reassemble byte-identical");
        // Default page size, and a JSON pointer into the stored result.
        let r = result_get(&c, 300, json!({"ref":id})).await;
        assert_eq!(
            r["result"]["content"][0]["text"].as_str().unwrap().len(),
            1024
        );
        let r = result_get(
            &c,
            301,
            json!({"ref":id,"pointer":"/structuredContent/count"}),
        )
        .await;
        assert_eq!(r["result"]["content"][0]["text"], "300", "{r}");

        // Another session cannot use the ref; a bogus ref reads the same.
        let other = Client::init(b.addr).await;
        let r = result_get(&other, 1, json!({"ref":id})).await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        let foreign = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            foreign.contains("call the original tool again"),
            "{foreign}"
        );
        let r = result_get(&other, 2, json!({"ref":"0".repeat(32)})).await;
        assert_eq!(r["result"]["content"][0]["text"], foreign.as_str());

        // Purged when the session ends.
        assert_eq!(router.result_store().usage().0, 1);
        assert_eq!(c.delete().await, 200);
        assert_eq!(router.result_store().usage().0, 0);
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_results_pass_and_filter_drops_output_schema() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(
                    MockServerSpec::new()
                        .tools(&["echo"])
                        .structured()
                        .big_items(50),
                ),
            )
            .unwrap();
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        let has_schema = |l: &Value, name: &str| {
            l["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap()
                .get("outputSchema")
                .is_some()
        };
        let l = c.rpc(1, "tools/list", json!({})).await;
        assert!(has_schema(&l, "m__structured"));

        router.set_policy(
            "m",
            Some(policy(
                "[servers.m.optimize]\nmax_result_bytes = 4096\n[servers.m.tools.structured]\nkeep = [\"/tool\"]\n[servers.m.tools.big]\nkeep = [\"/count\"]\n",
            )),
        );
        let l = c.rpc(2, "tools/list", json!({})).await;
        assert!(!has_schema(&l, "m__structured"), "{l}");
        let r = c.call(3, "big", json!({})).await;
        assert_eq!(r["result"]["structuredContent"], json!({"count":50}));
        assert_eq!(r["result"]["content"][0]["text"], "[]");
        assert_eq!(last_text(&r), "[mcpdoze: fields filtered]");
        // An unfiltered small result is untouched.
        let e = c.call(4, "echo", json!({"a":1})).await;
        assert_eq!(e["result"]["content"].as_array().unwrap().len(), 1, "{e}");
        b.finish().await;
    })
    .await;
}

// ---- toon (6b) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_is_smaller_lossless_and_falls_back() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(MockServerSpec::new().tools(&["echo"]).big_items(50)),
            )
            .unwrap();
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        let raw = c.call(1, "big", json!({})).await;
        let raw_text = raw["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();

        router.set_policy(
            "m",
            Some(policy("[servers.m.optimize]\nencode = \"toon\"\n")),
        );
        let r = c.call(2, "big", json!({})).await;
        let t = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(t.starts_with("[50]{id,name,active}:"), "{t}");
        assert!(t.len() * 5 <= raw_text.len() * 4);
        let back = mcpdoze_core::router::optimize::toon::decode(t).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(&raw_text).unwrap());
        assert_eq!(
            r["result"]["structuredContent"],
            raw["result"]["structuredContent"]
        );

        // Not smaller (quoted numeric strings): the original is kept.
        let e = c.call(3, "echo", json!({"a":["1","2"]})).await;
        let text = e["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with('{'), "{text}");
        b.finish().await;
    })
    .await;
}

// ---- session result cache (5) ----

fn cache_router(on_hit: &str, ttl: &str) -> Arc<Router> {
    let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
    router
        .register(
            "m",
            "m",
            spec(
                MockServerSpec::new()
                    .tools(&["counter", "write", "info"])
                    .read_only(&["counter", "info"]),
            ),
        )
        .unwrap();
    router.set_policy(
        "m",
        Some(policy(&format!(
            "[servers.m.optimize]\ncache = {{ ttl = \"{ttl}\", tools = [\"counter\", \"write\"], on_hit = \"{on_hit}\" }}\n"
        ))),
    );
    router
}

fn counter_value(r: &Value) -> u64 {
    r["result"]["structuredContent"]["value"]
        .as_u64()
        .unwrap_or_else(|| panic!("no counter value in {r}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_serves_within_ttl_and_invalidates_on_write_and_respawn() {
    with_timeout(LIMIT, async {
        let router = cache_router("full", "30s");
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;

        let first = counter_value(&c.call(2, "counter", json!({})).await);
        let second = counter_value(&c.call(3, "counter", json!({})).await);
        assert_eq!(first, second, "same value within the TTL");
        // Different arguments are a different key.
        let other = counter_value(&c.call(4, "counter", json!({"x": 1})).await);
        assert_ne!(other, first);

        // `write` is not read-only (never cached, even though listed) and
        // invalidates the server.
        let w1 = c.call(5, "write", json!({})).await;
        let w2 = c.call(6, "write", json!({})).await;
        assert_ne!(w1["result"], Value::Null);
        assert_eq!(router.result_cache().usage().0, 0);
        let after = counter_value(&c.call(7, "counter", json!({})).await);
        assert!(after > other, "{after} after write");
        assert_eq!(counter_value(&c.call(8, "counter", json!({})).await), after);
        drop((w1, w2));

        // Respawn: kill the backend; the cached value must not come back.
        let pid = c.call(9, "info", json!({})).await["result"]["structuredContent"]["pid"]
            .as_u64()
            .unwrap();
        harness::kill(u32::try_from(pid).unwrap());
        harness::wait_until("cache emptied by respawn", Duration::from_secs(20), || {
            router.result_cache().usage().0 == 0
        })
        .await;
        // The crash puts the backend in a short backoff: retry until served.
        let mut r = c.call(10, "counter", json!({})).await;
        for n in 0..50 {
            if r.get("error").is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            r = c.call(11 + n, "counter", json!({})).await;
        }
        assert_eq!(counter_value(&r), 1, "fresh process starts counting again");
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_expires_after_ttl() {
    with_timeout(LIMIT, async {
        let router = cache_router("full", "1s");
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;
        let first = counter_value(&c.call(2, "counter", json!({})).await);
        assert_eq!(counter_value(&c.call(3, "counter", json!({})).await), first);
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(counter_value(&c.call(4, "counter", json!({})).await) > first);
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_ignores_tools_not_listed_or_not_read_only() {
    with_timeout(LIMIT, async {
        // `counter` listed in cache.tools but not read-only: never cached.
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register("m", "m", spec(MockServerSpec::new().tools(&["counter"])))
            .unwrap();
        router.set_policy(
            "m",
            Some(policy(
                "[servers.m.optimize]\ncache = { ttl = \"30s\", tools = [\"counter\"] }\n",
            )),
        );
        let b = Broker::start(router.clone()).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;
        let a = counter_value(&c.call(2, "counter", json!({})).await);
        let a2 = counter_value(&c.call(3, "counter", json!({})).await);
        assert!(a2 > a);
        assert_eq!(router.result_cache().usage().0, 0);
        b.finish().await;

        // Read-only but not in cache.tools: not cached either.
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(
                    MockServerSpec::new()
                        .tools(&["counter"])
                        .read_only(&["counter"]),
                ),
            )
            .unwrap();
        router.set_policy(
            "m",
            Some(policy(
                "[servers.m.optimize]\ncache = { ttl = \"30s\", tools = [\"other\"] }\n",
            )),
        );
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;
        let a = counter_value(&c.call(2, "counter", json!({})).await);
        assert!(counter_value(&c.call(3, "counter", json!({})).await) > a);
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_is_per_session() {
    with_timeout(LIMIT, async {
        let router = cache_router("full", "30s");
        let b = Broker::start(router).await;
        let c1 = Client::init(b.addr).await;
        let c2 = Client::init(b.addr).await;
        c1.rpc(1, "tools/list", json!({})).await;
        let a = counter_value(&c1.call(2, "counter", json!({})).await);
        assert_eq!(counter_value(&c1.call(3, "counter", json!({})).await), a);
        let other = counter_value(&c2.call(2, "counter", json!({})).await);
        assert_ne!(other, a, "a second session must not see the first's entry");
        assert_eq!(
            counter_value(&c2.call(3, "counter", json!({})).await),
            other
        );
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_reference_mode_returns_a_stub_with_a_working_ref() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(
                    MockServerSpec::new()
                        .tools(&["echo"])
                        .big_items(40)
                        .read_only(&["big"]),
                ),
            )
            .unwrap();
        router.set_policy(
            "m",
            Some(policy(
                "[servers.m.optimize]
cache = { ttl = \"30s\", tools = [\"big\"], on_hit = \"reference\" }
",
            )),
        );
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        let names = tool_names(&c.rpc(1, "tools/list", json!({})).await);
        assert!(
            names.contains(&"mcpdoze__result_get".to_owned()),
            "{names:?}"
        );
        let first = c.call(2, "big", json!({})).await;
        let full = first["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let hit = c.call(3, "big", json!({})).await;
        let stub = hit["result"]["content"][0]["text"].as_str().unwrap();
        assert!(stub.contains("equals an earlier result"), "{stub}");
        let id = stub
            .split("ref=")
            .nth(1)
            .unwrap()
            .trim_end_matches(&[']', '.'][..])
            .to_owned();
        let r = result_get(&c, 4, json!({"ref": id})).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        let got = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(full.starts_with(got) && !got.is_empty(), "{r}");
        b.finish().await;
    })
    .await;
}

// ---- batch meta-tool (8) ----

fn batch_router(mode: &str, extra: &str) -> Arc<Router> {
    let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
    router
        .register(
            "m",
            "m",
            spec(
                MockServerSpec::new()
                    .tools(&["counter", "write", "info", "secret"])
                    .read_only(&["counter", "info", "secret"]),
            ),
        )
        .unwrap();
    router.set_policy(
        "m",
        Some(policy(&format!(
            "[servers.m.optimize]\nbatch = \"{mode}\"\n{extra}"
        ))),
    );
    router
}

async fn batch(c: &Client, id: i64, calls: Value) -> Value {
    c.rpc(
        id,
        "tools/call",
        json!({"name":"mcpdoze__batch","arguments":{"calls":calls}}),
    )
    .await
}

fn block_text(r: &Value, i: usize) -> String {
    r["result"]["content"][i]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no block {i} in {r}"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_three_read_only_calls_is_three_events_and_one_process() {
    with_timeout(LIMIT, async {
        let logdir = tempfile::tempdir().unwrap();
        let router = batch_router("read-only", "");
        let st = mcpdoze_core::stats::Stats::start(
            &mcpdoze_core::config::StatsConfig::default(),
            logdir.path().to_path_buf(),
        )
        .unwrap();
        router.set_stats(st.clone());
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        let list = c.rpc(1, "tools/list", json!({})).await;
        let defs = list["result"]["tools"].as_array().unwrap();
        let def = defs
            .iter()
            .find(|t| t["name"] == "mcpdoze__batch")
            .expect("batch listed");
        assert!(def.get("outputSchema").is_none());

        let r = batch(
            &c,
            2,
            json!([
                {"name":"m__info"},
                {"name":"m__counter","arguments":{}},
                {"name":"m__info","arguments":{}}
            ]),
        )
        .await;
        assert_eq!(r["result"]["content"].as_array().unwrap().len(), 3, "{r}");
        let pids: Vec<u64> = [0, 2]
            .iter()
            .map(|&i| {
                let t = block_text(&r, i);
                assert!(
                    t.starts_with(&format!("[{i}] m__info isError=false\n")),
                    "{t}"
                );
                let body: Value = serde_json::from_str(t.split_once('\n').unwrap().1).unwrap();
                body["pid"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(pids[0], pids[1], "one backend process");
        assert!(block_text(&r, 1).starts_with("[1] m__counter isError=false"));

        assert!(st.flush(Duration::from_secs(5)));
        let mut text = String::new();
        for e in std::fs::read_dir(logdir.path()).unwrap() {
            text.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap_or_default());
        }
        let events: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let calls = |server: &str| {
            events
                .iter()
                .filter(|e| e["ev"] == "call" && e["server"] == server)
                .count()
        };
        assert_eq!(calls("m"), 3, "{text}");
        assert_eq!(calls("mcpdoze"), 1, "batch recorded once: {text}");
        assert_eq!(
            events.iter().filter(|e| e["ev"] == "spawn").count(),
            1,
            "{text}"
        );
        let tagged = events
            .iter()
            .filter(|e| {
                e["opt"]
                    .as_array()
                    .is_some_and(|o| o.iter().any(|t| t == "batch"))
            })
            .count();
        assert_eq!(tagged, 1, "{text}");
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_refuses_writes_under_read_only_and_allows_them_under_all() {
    with_timeout(LIMIT, async {
        for (mode, allowed) in [("read-only", false), ("all", true)] {
            let b = Broker::start(batch_router(mode, "")).await;
            let c = Client::init(b.addr).await;
            c.rpc(1, "tools/list", json!({})).await;
            let r = batch(&c, 2, json!([{"name":"m__write"},{"name":"m__counter"}])).await;
            let (w, ok) = (block_text(&r, 0), block_text(&r, 1));
            assert!(ok.contains("isError=false"), "{ok}");
            if allowed {
                assert!(w.contains("isError=false"), "{w}");
            } else {
                assert!(
                    w.contains("isError=true") && w.contains("readOnlyHint"),
                    "{w}"
                );
            }
            b.finish().await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_is_not_listed_when_off_and_rejects_nesting_and_overflow() {
    with_timeout(LIMIT, async {
        let off = Broker::start(batch_router("off", "")).await;
        let c = Client::init(off.addr).await;
        let names = tool_names(&c.rpc(1, "tools/list", json!({})).await);
        assert!(!names.contains(&"mcpdoze__batch".to_owned()), "{names:?}");
        off.finish().await;

        let b = Broker::start(batch_router("all", "")).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;
        let nested =
            json!([{"name":"mcpdoze__batch","arguments":{"calls":[{"name":"m__counter"}]}}]);
        let r = batch(&c, 2, nested).await;
        let t = block_text(&r, 0);
        assert!(t.contains("isError=true") && t.contains("nested"), "{t}");

        let many: Vec<Value> = (0..17).map(|_| json!({"name":"m__counter"})).collect();
        let r = batch(&c, 3, Value::Array(many)).await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(block_text(&r, 0).contains("at most 16"), "{r}");
        let r = batch(&c, 4, json!([])).await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_refuses_a_denied_tool() {
    with_timeout(LIMIT, async {
        let b = Broker::start(batch_router("all", "tools_deny = [\"secret\"]\n")).await;
        let c = Client::init(b.addr).await;
        c.rpc(1, "tools/list", json!({})).await;
        let r = batch(&c, 2, json!([{"name":"m__secret"},{"name":"m__counter"}])).await;
        let denied = block_text(&r, 0);
        assert!(
            denied.contains("isError=true") && denied.contains("unknown tool"),
            "{denied}"
        );
        assert!(block_text(&r, 1).contains("isError=false"));
        b.finish().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_batch_cancels_its_inner_calls() {
    with_timeout(LIMIT, async {
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        router
            .register(
                "m",
                "m",
                spec(
                    MockServerSpec::new()
                        .tools(&["slow"])
                        .read_only(&["slow"])
                        .delay_ms(20_000),
                ),
            )
            .unwrap();
        router.set_policy(
            "m",
            Some(policy("[servers.m.optimize]\nbatch = \"read-only\"\n")),
        );
        let b = Broker::start(router.clone()).await;
        let c = Arc::new(Client::init(b.addr).await);
        c.rpc(1, "tools/list", json!({})).await;
        let inst = router.server("m").unwrap().instance.clone();
        let c2 = c.clone();
        let call = tokio::spawn(async move {
            batch(&c2, 5, json!([{"name":"m__slow"},{"name":"m__slow"}])).await
        });
        harness::wait_until("inner calls pending", Duration::from_secs(10), || {
            inst.pending_calls() == 2
        })
        .await;
        let n = c
            .post(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                "params":{"requestId":5,"reason":"test"}}))
            .await;
        assert_eq!(n.status, 202);
        let r = call.await.unwrap();
        assert_eq!(r["error"]["code"], -32800, "{r}");
        harness::wait_until("inner calls cancelled", Duration::from_secs(10), || {
            inst.pending_calls() == 0
        })
        .await;
        b.finish().await;
    })
    .await;
}

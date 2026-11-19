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

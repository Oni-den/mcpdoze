// SPDX-License-Identifier: GPL-3.0-or-later

//! Statistics log end to end: a per-cwd mock server called through the
//! broker produces `session`, `spawn`, `call`, `sample` and `unload` events,
//! the summary counts them, and the file holds no tool arguments, results,
//! directories or session ids.

#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{wait_until, with_timeout, Broker, Client};
use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::config::{Scope, StatsConfig};
use mcpdoze_core::router::{CatalogOptions, MemoryCatalog, Router, ScopeOptions};
use mcpdoze_core::stats::{self, Stats};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const LIMIT: Duration = Duration::from_secs(90);
/// A private tool argument; must never reach the log.
const SECRET_ARG: &str = "private-argument-value-7f3e1c";
const RESULT_MARKER: &str = "result-marker-b81d";

fn read_log(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            out.push_str(&std::fs::read_to_string(p).unwrap());
        }
    }
    out
}

fn events(text: &str) -> Vec<Value> {
    text.lines()
        .map(|l| serde_json::from_str(l).expect("every line is JSON"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_calls_are_logged_without_private_data() {
    with_timeout(LIMIT, async {
        let logdir = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        let spec = BackendSpec::new(BIN, MockServerSpec::new().tools(&["echo"]).args())
            .idle_ttl(Duration::from_millis(1500))
            .start_timeout(Duration::from_secs(20));
        router
            .register_scoped(
                "mock",
                "m",
                spec,
                CatalogOptions::default(),
                ScopeOptions {
                    scope: Scope::PerCwd,
                    max_instances: 4,
                },
            )
            .unwrap();
        let cfg = StatsConfig {
            sample_interval_secs: 1,
            debug: true,
            ..StatsConfig::default()
        };
        let st = Stats::start(&cfg, logdir.path().to_path_buf()).unwrap();
        router.set_stats(st.clone());
        let broker = Broker::start(router.clone()).await;

        let cwd = workdir.path().to_string_lossy().into_owned();
        let c = Client::init_with(broker.addr, &[("X-Mcpdoze-Cwd", &cwd)]).await;
        for id in 1..=3 {
            let r = c
                .call(
                    id,
                    "echo",
                    json!({"api_key": SECRET_ARG, "note": RESULT_MARKER}),
                )
                .await;
            assert!(r["result"].is_object(), "{r}");
        }
        // One sample while running, then the idle unload.
        let dir = logdir.path().to_path_buf();
        wait_until("sample and idle unload", Duration::from_secs(20), || {
            st.flush(Duration::from_secs(1));
            let t = read_log(&dir);
            t.contains("\"ev\":\"unload\"") && t.contains("\"est_direct_mb\"")
        })
        .await;
        assert_eq!(c.delete().await, 200);
        assert!(st.flush(Duration::from_secs(5)));

        let text = read_log(logdir.path());
        for private in [SECRET_ARG, RESULT_MARKER, cwd.as_str(), c.session.as_str()] {
            assert!(!text.contains(private), "log leaks {private:?}:\n{text}");
        }
        let evs = events(&text);
        let of = |ev: &str| evs.iter().filter(|e| e["ev"] == ev).collect::<Vec<_>>();
        let calls = of("call");
        assert_eq!(calls.len(), 3, "{text}");
        assert_eq!(calls[0]["server"], "mock");
        assert_eq!(calls[0]["kind"], "tool");
        assert_eq!(calls[0]["name"], "echo");
        assert_eq!(calls[0]["ok"], true);
        assert_eq!(calls[0]["cold_start"], true);
        assert_eq!(calls[1]["cold_start"], false);
        assert_eq!(calls[0]["scope"], "per-cwd");
        assert_eq!(calls[0]["era"], "legacy");
        assert_eq!(
            calls[0]["instance"],
            stats::short_hash(&mcpdoze_core::router::cwd_key(workdir.path()))
        );
        assert!(calls[0]["bytes_in"].as_u64().unwrap() > 0);
        assert!(calls[0]["req_id"].is_u64(), "debug level adds ids");
        assert!(calls[0]["backend_ms"].is_u64());
        let spawn = of("spawn");
        assert_eq!(spawn.len(), 1, "{text}");
        assert_eq!(spawn[0]["generation"], 1);
        let unload = of("unload");
        assert_eq!(unload[0]["reason"], "idle");
        let sessions = of("session");
        assert_eq!(sessions.first().unwrap()["action"], "open");
        assert_eq!(sessions.last().unwrap()["action"], "close");
        let sample = of("sample")
            .into_iter()
            .find(|s| {
                s["servers"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|x| x["sessions"] == 1))
            })
            .cloned()
            .expect("a sample of the window with the calls");
        let m = &sample["servers"][0];
        assert_eq!(m["server"], "mock");
        assert!(m["private_mb"].as_f64().unwrap() > 0.0, "{sample}");
        assert!(sample["actual_mb"].as_f64().unwrap() > 0.0);

        let s = stats::summarize_dir(logdir.path(), 0, stats::now_ms() + 1000).unwrap();
        let ms = &s.servers[0];
        assert_eq!((ms.calls, ms.cold_starts, ms.spawns), (3, 1, 1));
        assert_eq!(ms.unloads.get("idle"), Some(&1));
        assert!(s.memory.samples >= 1);

        broker.finish().await;
    })
    .await;
}

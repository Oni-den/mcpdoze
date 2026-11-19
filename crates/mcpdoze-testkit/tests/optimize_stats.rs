// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase A0 acceptance: the per-tool table ranks the `big` tool first, a
//! repeated identical call is flagged `dup`, the tool list size is known,
//! and the stats file holds no argument hash or argument value.

#[path = "support/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{with_timeout, Broker, Client};
use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::config::StatsConfig;
use mcpdoze_core::router::{MemoryCatalog, Router};
use mcpdoze_core::stats::{self, Stats};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const LIMIT: Duration = Duration::from_secs(90);
const ARG_MARKER: &str = "argument-marker-4d2a9e";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_table_dup_and_list_bytes() {
    with_timeout(LIMIT, async {
        let logdir = tempfile::tempdir().unwrap();
        let router = Router::with(Arc::new(MemoryCatalog::default()), harness::tuning());
        let spec = BackendSpec::new(
            BIN,
            MockServerSpec::new().tools(&["echo"]).big_items(200).args(),
        )
        .start_timeout(Duration::from_secs(20));
        router.register("mock", "m", spec).unwrap();
        let st = Stats::start(&StatsConfig::default(), logdir.path().to_path_buf()).unwrap();
        router.set_stats(st.clone());
        let broker = Broker::start(router.clone()).await;
        let c = Client::init(broker.addr).await;

        let args = json!({"note": ARG_MARKER});
        for id in 1..=2 {
            let r = c.call(id, "echo", args.clone()).await;
            assert!(r["result"].is_object(), "{r}");
        }
        let r = c.call(3, "echo", json!({"note": "other"})).await;
        assert!(r["result"].is_object(), "{r}");
        for id in 4..=5 {
            let r = c.call(id, "big", json!({})).await;
            assert!(r["result"].is_object(), "{r}");
        }
        assert!(c.rpc(6, "tools/list", json!({})).await["result"]["tools"].is_array());
        assert!(st.flush(Duration::from_secs(5)));

        // File: dup flags only, no argument value and no hash.
        let mut text = String::new();
        for e in std::fs::read_dir(logdir.path()).unwrap() {
            text.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap_or_default());
        }
        assert!(!text.contains(ARG_MARKER), "{text}");
        assert!(
            !text.contains("args_hash") && !text.contains("hash"),
            "{text}"
        );
        let calls: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["ev"] == "call")
            .collect();
        assert_eq!(calls.len(), 5);
        let dups: Vec<bool> = calls.iter().map(|e| e["dup"] == true).collect();
        assert_eq!(dups, [false, true, false, false, true], "{text}");

        let until = stats::now_ms() + 1000;
        let mut summary = stats::summarize_dir(logdir.path(), 0, until).unwrap();
        assert_eq!(summary.tools[0].tool, "big", "{:?}", summary.tools);
        assert_eq!(summary.tools[0].calls, 2);
        assert!(summary.tools[0].share > 0.9);
        assert_eq!(summary.tools[0].dup_calls, 1);
        assert_eq!(summary.tools[1].tool, "echo");
        assert_eq!(summary.tools[1].dup_calls, 1);
        assert_eq!(
            summary.tools[0].est_tokens,
            summary.tools[0].total_bytes / 4
        );
        summary.lists.clear();

        let sizes = router.tool_list_bytes();
        assert!(sizes["mock"] > 0, "{sizes:?}");
        drop(c);
        broker.finish().await;
    })
    .await;
}

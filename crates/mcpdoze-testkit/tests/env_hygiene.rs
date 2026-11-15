// SPDX-License-Identifier: GPL-3.0-or-later

//! The broker token never reaches a stdio backend through environment
//! inheritance. The outer test re-runs this test binary with the token
//! variables set (so the test process's own environment is never mutated);
//! the inner test then spawns the mock and asks it for its environment.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::{BackendSpec, CallMeta, Instance, NoEvents};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use tokio::time::timeout;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
/// Marks the re-executed child run.
const CHILD: &str = "MCPDOZE_TEST_ENV_HYGIENE_CHILD";
/// A custom `broker.token_env` name.
const CUSTOM: &str = "MCPDOZE_TEST_CUSTOM_BROKER_TOKEN";
/// Placeholder token value (not a real secret).
const VALUE: &str = "placeholder-token-value";

#[test]
fn token_never_reaches_backend() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "inner_backend_env",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("MCPDOZE_TOKEN", VALUE)
        .env(CUSTOM, VALUE)
        .env("MCPDOZE_TEST_KEEP", "kept")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "inner run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "inner test did not run:\n{stdout}"
    );
}

async fn backend_env(spec: BackendSpec) -> Value {
    let inst = Instance::new("m", spec, Arc::new(NoEvents));
    let env = {
        let guard = inst.acquire().await.unwrap();
        let (_, rx) = guard
            .conn()
            .begin(
                "tools/call",
                json!({"name": "info", "arguments": {}}),
                CallMeta::default(),
            )
            .await
            .unwrap();
        let resp = rx.await.unwrap().unwrap();
        resp["result"]["structuredContent"]["env"].clone()
    };
    inst.shutdown().await;
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inner_backend_env() {
    if std::env::var_os(CHILD).is_none() {
        return; // Only meaningful in the re-executed child.
    }
    timeout(Duration::from_secs(60), async {
        let mock = MockServerSpec::new()
            .tools(&["info"])
            .echo_env("MCPDOZE_TOKEN")
            .echo_env(CUSTOM);
        let base = || BackendSpec::new(BIN, mock.args()).start_timeout(Duration::from_secs(20));

        // Default spec: MCPDOZE_TOKEN stripped, unrelated variables kept.
        let env = backend_env(base()).await;
        assert!(env.get("MCPDOZE_TOKEN").is_none(), "{env}");
        assert_eq!(env["MCPDOZE_TEST_KEEP"], "kept", "{env}");
        assert_eq!(env[CUSTOM], VALUE, "not stripped unless named: {env}");

        // A custom `broker.token_env` variable is stripped as well.
        let mut s = base();
        s.strip_env.insert(CUSTOM.into());
        let env = backend_env(s).await;
        assert!(env.get("MCPDOZE_TOKEN").is_none(), "{env}");
        assert!(env.get(CUSTOM).is_none(), "{env}");

        // An explicit `env` entry configured by the user still wins.
        let mut s = base();
        s.env.insert("MCPDOZE_TOKEN".into(), "explicit".into());
        let env = backend_env(s).await;
        assert_eq!(env["MCPDOZE_TOKEN"], "explicit", "{env}");
    })
    .await
    .expect("test timed out");
}

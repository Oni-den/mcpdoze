// SPDX-License-Identifier: GPL-3.0-or-later

//! Mock `headers_helper` for tests. Prints `{"<name>": "<value>"}` where the
//! value is the n-th entry of `--values a,b,c` (the last one once they run
//! out) and n is the number of earlier runs recorded in `--counter FILE`.
//!
//! Flags: `--counter FILE` (run counter, incremented on every run),
//! `--values a,b` (default `x`), `--name NAME` (default `Authorization`),
//! `--raw TEXT` (print TEXT instead of JSON), `--exit N` (exit status).

use std::path::PathBuf;

fn main() {
    let mut counter: Option<PathBuf> = None;
    let mut values = vec!["x".to_owned()];
    let mut name = "Authorization".to_owned();
    let mut raw: Option<String> = None;
    let mut exit = 0;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--counter" => counter = it.next().map(PathBuf::from),
            "--values" => {
                values = it
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_owned)
                    .collect()
            }
            "--name" => name = it.next().unwrap_or_default(),
            "--raw" => raw = it.next(),
            "--exit" => exit = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    let mut n = 0usize;
    if let Some(c) = &counter {
        n = std::fs::read_to_string(c)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let _ = std::fs::write(c, (n + 1).to_string());
    }
    match raw {
        Some(r) => print!("{r}"),
        None => {
            let v = values.get(n).or(values.last()).cloned().unwrap_or_default();
            let mut m = serde_json::Map::new();
            m.insert(name, serde_json::Value::String(v));
            print!("{}", serde_json::Value::Object(m));
        }
    }
    std::process::exit(exit);
}

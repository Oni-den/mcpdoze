// SPDX-License-Identifier: GPL-3.0-or-later

//! Tracing capture for tests that assert on log output.
//!
//! A thread-scoped subscriber (`tracing::subscriber::set_default`) is not
//! reliable when other tests of the same binary run in parallel: while it is
//! the only registered dispatcher, tracing-core computes each callsite's
//! cached interest from the default of whichever thread reaches the callsite
//! first. A callsite first hit by another test's thread (which has no
//! subscriber) is cached as "never", and the capturing test then silently
//! loses those events, which could also hide a leaked secret.
//!
//! Instead one global subscriber is installed per test binary. Its dynamic
//! filter asks per event whether the emitting thread has an active
//! [`Capture`], so interest is never cached as "never" (a plain `filter_fn`
//! would cache its first answer per callsite), and parallel tests neither
//! drop nor pollute the capture. Only events emitted on the capturing thread are
//! recorded, so capturing tests run on a `current_thread` runtime.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// Upper bound per capture, so a runaway logger cannot exhaust memory.
const LIMIT: usize = 16 * 1024 * 1024;

type Buf = Arc<Mutex<Vec<u8>>>;

thread_local! {
    static SINK: RefCell<Option<Buf>> = const { RefCell::new(None) };
}

fn current_sink() -> Option<Buf> {
    SINK.try_with(|s| s.borrow().clone()).ok().flatten()
}

fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(ThreadSink)
            .with_filter(dynamic_filter_fn(|_, _| current_sink().is_some()));
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .expect("no other global tracing subscriber in this test binary");
    });
}

/// Records every event (TRACE and up) emitted on the current thread until
/// dropped.
pub struct Capture(Buf);

impl Capture {
    pub fn start() -> Self {
        install();
        let buf = Buf::default();
        SINK.with(|s| {
            let mut s = s.borrow_mut();
            assert!(s.is_none(), "nested log capture on one thread");
            *s = Some(buf.clone());
        });
        Self(buf)
    }

    /// Everything captured so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = SINK.try_with(|s| s.borrow_mut().take());
    }
}

/// Writes formatted events into the emitting thread's capture, if any.
struct ThreadSink;

struct SinkWriter(Option<Buf>);

impl std::io::Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(sink) = &self.0 {
            let mut v = sink.lock().unwrap();
            if v.len() < LIMIT {
                v.extend_from_slice(buf);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ThreadSink {
    type Writer = SinkWriter;
    fn make_writer(&'a self) -> SinkWriter {
        SinkWriter(current_sink())
    }
}

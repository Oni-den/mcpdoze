// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-instance rings: the last frames seen on the backend connection and
//! the last lifecycle transitions.
//!
//! Entries are fixed-size `Copy` values ([`FrameMeta`], [`Transition`]) that
//! hold no text from the wire: a method is reduced to a [`MethodTag`] from a
//! fixed table, everything else is a number or a `&'static str`. Rings start
//! empty and unallocated, so an instance whose tap is off never allocates.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

use super::classify::Signature;
use super::record::{FrameRec, Name, TransitionRec};

/// Frames kept per instance (default).
pub const DEFAULT_FRAMES: usize = 32;
/// Lifecycle transitions kept per instance.
pub const TRANSITIONS: usize = 16;

/// Direction of a frame, from the broker's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Broker to backend.
    Out,
    /// Backend to broker.
    In,
}

/// JSON-RPC frame class (or non-JSON noise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameType {
    /// A request.
    Request,
    /// A successful response.
    Response,
    /// An error response.
    Error,
    /// A notification.
    Notification,
    /// A line that is not JSON.
    Noise,
    /// JSON that is not a valid JSON-RPC message.
    Invalid,
    /// A line over the size limit.
    Oversize,
}

const METHODS: &[&str] = &[
    "initialize",
    "notifications/initialized",
    "ping",
    "tools/list",
    "tools/call",
    "prompts/list",
    "prompts/get",
    "resources/list",
    "resources/read",
    "resources/templates/list",
    "resources/subscribe",
    "resources/unsubscribe",
    "logging/setLevel",
    "completion/complete",
    "notifications/cancelled",
    "notifications/progress",
    "notifications/message",
    "notifications/tools/list_changed",
    "notifications/prompts/list_changed",
    "notifications/resources/list_changed",
    "notifications/resources/updated",
    "sampling/createMessage",
    "roots/list",
    "elicitation/create",
];

/// A method name reduced to an index into a fixed table; anything unknown
/// is `other`. One byte, no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodTag(u8);

impl MethodTag {
    /// No method (responses, noise).
    pub const NONE: MethodTag = MethodTag(254);
    const OTHER: MethodTag = MethodTag(255);

    /// Maps a method name.
    pub fn of(method: &str) -> Self {
        if method.is_empty() {
            return Self::NONE;
        }
        match METHODS.iter().position(|m| *m == method) {
            Some(i) => MethodTag(i as u8),
            None => Self::OTHER,
        }
    }

    /// The method name, `other`, or `None` for [`MethodTag::NONE`].
    pub fn name(self) -> Option<&'static str> {
        match self.0 {
            254 => None,
            255 => Some("other"),
            i => METHODS.get(i as usize).copied(),
        }
    }
}

/// One frame, fixed size and `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeta {
    /// Milliseconds since the ring was created.
    pub at_ms: u64,
    /// Direction.
    pub dir: Dir,
    /// Class.
    pub ftype: FrameType,
    /// Method (requests and notifications).
    pub method: MethodTag,
    /// Backend-side request id (0 when none).
    pub bid: u64,
    /// Frame size in bytes (saturating).
    pub bytes: u32,
    /// Noise signature, for [`FrameType::Noise`].
    pub sig: Option<Signature>,
}

/// One lifecycle transition.
#[derive(Debug, Clone, Copy)]
pub struct Transition {
    /// Milliseconds since the ring was created.
    pub at_ms: u64,
    /// State entered.
    pub to: &'static str,
    /// Why.
    pub reason: &'static str,
    /// Calls in flight at that moment.
    pub in_flight: u32,
}

struct Ring<T> {
    epoch: Instant,
    cap: usize,
    buf: Mutex<VecDeque<T>>,
}

impl<T: Copy> Ring<T> {
    fn new(cap: usize) -> Self {
        Self {
            epoch: Instant::now(),
            cap: cap.max(1),
            buf: Mutex::new(VecDeque::new()),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn push(&self, v: T) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if b.len() >= self.cap {
            b.pop_front();
        }
        b.push_back(v);
    }

    fn items(&self) -> Vec<T> {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        b.iter().copied().collect()
    }
}

/// Ring of the last frames of one backend connection.
pub struct FrameRing(Ring<FrameMeta>);

impl FrameRing {
    /// A ring of `cap` frames.
    pub fn new(cap: usize) -> Self {
        Self(Ring::new(cap))
    }

    /// Appends a frame (the oldest is dropped when full).
    pub fn push(
        &self,
        dir: Dir,
        ftype: FrameType,
        method: MethodTag,
        bid: u64,
        bytes: usize,
        sig: Option<Signature>,
    ) {
        self.0.push(FrameMeta {
            at_ms: self.0.now_ms(),
            dir,
            ftype,
            method,
            bid,
            bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
            sig,
        });
    }

    /// Age in ms of the newest frame in direction `dir`, if any.
    pub fn last_age_ms(&self, dir: Dir) -> Option<u64> {
        let now = self.0.now_ms();
        self.0
            .items()
            .iter()
            .rev()
            .find(|f| f.dir == dir)
            .map(|f| now.saturating_sub(f.at_ms))
    }

    /// The frames, oldest first, with `dt_ms` negative relative to now.
    pub fn snapshot(&self) -> Vec<FrameRec> {
        let now = self.0.now_ms();
        self.0
            .items()
            .into_iter()
            .map(|f| FrameRec {
                dt_ms: f.at_ms as i64 - now as i64,
                dir: f.dir,
                ftype: f.ftype,
                method: f.method.name().map(Name::new),
                bid: (f.bid != 0).then_some(f.bid),
                bytes: f.bytes,
                sig: f.sig,
            })
            .collect()
    }

    /// Frames currently held.
    pub fn len(&self) -> usize {
        self.0.buf.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Allocated capacity (0 until the first frame; used to show that an
    /// off tap never allocates).
    pub fn capacity(&self) -> usize {
        self.0
            .buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capacity()
    }
}

/// Ring of the last [`TRANSITIONS`] lifecycle transitions.
pub struct TransitionRing(Ring<Transition>);

impl Default for TransitionRing {
    fn default() -> Self {
        Self(Ring::new(TRANSITIONS))
    }
}

impl TransitionRing {
    /// Appends a transition.
    pub fn push(&self, to: &'static str, reason: &'static str, in_flight: usize) {
        self.0.push(Transition {
            at_ms: self.0.now_ms(),
            to,
            reason,
            in_flight: u32::try_from(in_flight).unwrap_or(u32::MAX),
        });
    }

    /// Transitions, oldest first.
    pub fn snapshot(&self) -> Vec<TransitionRec> {
        let now = self.0.now_ms();
        self.0
            .items()
            .into_iter()
            .map(|t| TransitionRec {
                to: t.to,
                age_ms: now.saturating_sub(t.at_ms),
                reason: t.reason,
                in_flight: t.in_flight,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ring_is_bounded_and_fixed_size() {
        assert!(std::mem::size_of::<FrameMeta>() <= 40);
        let r = FrameRing::new(3);
        assert!(r.is_empty());
        for i in 0..10u64 {
            r.push(
                Dir::Out,
                FrameType::Request,
                MethodTag::of("tools/call"),
                i + 1,
                10,
                None,
            );
        }
        let s = r.snapshot();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].bid, Some(8));
        assert_eq!(s[2].bid, Some(10));
        assert!(s.iter().all(|f| f.dt_ms <= 0));
        assert_eq!(s[0].method.as_ref().map(Name::as_str), Some("tools/call"));
    }

    #[test]
    fn unknown_methods_are_not_kept() {
        let t = MethodTag::of("x/secret-method-name");
        assert_eq!(t.name(), Some("other"));
        assert_eq!(MethodTag::of("").name(), None);
    }

    #[test]
    fn transitions_keep_the_last_sixteen() {
        let r = TransitionRing::default();
        for _ in 0..40 {
            r.push("running", "spawn", 1);
        }
        assert_eq!(r.snapshot().len(), TRANSITIONS);
    }
}

//! `ut-notify`: notification dispatch (Section 21 "System notifications").
//!
//! Defines the set of notification kinds the spec lists, a suppression-rule
//! mechanism ("Notification suppression rules"), a bounded in-memory
//! history ("Notification history"), and a [`Sink`] trait so the actual
//! delivery mechanism (a desktop notification, a sound, a webhook POST) is
//! pluggable and platform-specific code lives outside this crate.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    CommandFinished,
    CommandFailed,
    LongRunningCommand,
    DownloadFinished,
    BuildFinished,
    PackageUpdate,
    SshDisconnected,
    SshReconnected,
    BatteryWarning,
    DiskSpaceWarning,
    ProcessCrash,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    /// Unix timestamp (seconds).
    pub timestamp: u64,
}

/// A delivery target for notifications. Implementations live outside this
/// crate (desktop toast, sound player, webhook client); this crate only
/// decides *whether* and *what* to deliver.
pub trait Sink {
    fn deliver(&mut self, notification: &Notification);
}

/// A suppression rule: notifications of `kind` are dropped entirely when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuppressionRule {
    pub kind: Kind,
}

pub struct NotificationCenter<S: Sink> {
    sink: S,
    suppressed: std::collections::HashSet<Kind>,
    history: VecDeque<Notification>,
    history_limit: usize,
    now_fn: fn() -> u64,
}

impl<S: Sink> NotificationCenter<S> {
    pub fn new(sink: S) -> Self {
        NotificationCenter { sink, suppressed: Default::default(), history: VecDeque::new(), history_limit: 200, now_fn: default_now }
    }

    #[cfg(test)]
    fn with_clock(sink: S, now_fn: fn() -> u64) -> Self {
        NotificationCenter { sink, suppressed: Default::default(), history: VecDeque::new(), history_limit: 200, now_fn }
    }

    pub fn suppress(&mut self, rule: SuppressionRule) {
        self.suppressed.insert(rule.kind);
    }

    pub fn unsuppress(&mut self, kind: Kind) {
        self.suppressed.remove(&kind);
    }

    pub fn is_suppressed(&self, kind: Kind) -> bool {
        self.suppressed.contains(&kind)
    }

    /// Attempt to send a notification. Returns `false` if it was suppressed.
    pub fn notify(&mut self, kind: Kind, title: impl Into<String>, body: impl Into<String>) -> bool {
        if self.suppressed.contains(&kind) {
            return false;
        }
        let n = Notification { kind, title: title.into(), body: body.into(), timestamp: (self.now_fn)() };
        self.sink.deliver(&n);
        self.history.push_back(n);
        while self.history.len() > self.history_limit {
            self.history.pop_front();
        }
        true
    }

    pub fn history(&self) -> impl Iterator<Item = &Notification> {
        self.history.iter()
    }

    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        while self.history.len() > self.history_limit {
            self.history.pop_front();
        }
    }
}

fn default_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A [`Sink`] that just collects notifications, useful for tests and for a
/// headless CLI that wants to print them instead of showing a toast.
#[derive(Default)]
pub struct CollectingSink {
    pub delivered: Vec<Notification>,
}

impl Sink for CollectingSink {
    fn deliver(&mut self, notification: &Notification) {
        self.delivered.push(notification.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_delivers_to_sink() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 42);
        let sent = center.notify(Kind::CommandFinished, "Done", "Build succeeded");
        assert!(sent);
        assert_eq!(center.sink.delivered.len(), 1);
        assert_eq!(center.sink.delivered[0].timestamp, 42);
    }

    #[test]
    fn suppressed_kind_is_dropped() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 0);
        center.suppress(SuppressionRule { kind: Kind::BatteryWarning });
        let sent = center.notify(Kind::BatteryWarning, "Low battery", "10% remaining");
        assert!(!sent);
        assert!(center.sink.delivered.is_empty());
    }

    #[test]
    fn unsuppress_restores_delivery() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 0);
        center.suppress(SuppressionRule { kind: Kind::BatteryWarning });
        center.unsuppress(Kind::BatteryWarning);
        assert!(center.notify(Kind::BatteryWarning, "Low battery", "10%"));
    }

    #[test]
    fn history_tracks_delivered_notifications() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 0);
        center.notify(Kind::BuildFinished, "Build", "ok");
        center.notify(Kind::CommandFailed, "Failed", "exit 1");
        assert_eq!(center.history().count(), 2);
    }

    #[test]
    fn history_respects_limit() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 0);
        center.set_history_limit(2);
        center.notify(Kind::CommandFinished, "a", "");
        center.notify(Kind::CommandFinished, "b", "");
        center.notify(Kind::CommandFinished, "c", "");
        let titles: Vec<&str> = center.history().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["b", "c"]);
    }

    #[test]
    fn suppressed_notifications_never_added_to_history() {
        let mut center = NotificationCenter::with_clock(CollectingSink::default(), || 0);
        center.suppress(SuppressionRule { kind: Kind::DiskSpaceWarning });
        center.notify(Kind::DiskSpaceWarning, "Low disk", "");
        assert_eq!(center.history().count(), 0);
    }
}

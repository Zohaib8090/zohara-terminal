//! `ut-history`: command history store and search (Section 7 "History").
//!
//! An in-memory, append-only log of commands with the metadata the spec's
//! search filters need (directory, exit status, profile, timestamp) plus
//! favorite/private flags and deduplication. Persistence (writing this to
//! disk, encrypted-history support) is intentionally left to whatever owns
//! the store's lifetime (`ut-profile`, or the app layer) — this crate is
//! just the data structure and the query logic over it.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub command: String,
    pub directory: String,
    pub profile: String,
    /// Unix timestamp (seconds) of when the command was run.
    pub timestamp: u64,
    pub exit_status: Option<i32>,
    pub favorite: bool,
    /// Excluded from search/export/sync, e.g. because it contained a secret.
    pub private: bool,
}

impl HistoryEntry {
    pub fn now(command: impl Into<String>, directory: impl Into<String>, profile: impl Into<String>) -> Self {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        HistoryEntry {
            command: command.into(),
            directory: directory.into(),
            profile: profile.into(),
            timestamp,
            exit_status: None,
            favorite: false,
            private: false,
        }
    }
}

/// A set of optional filters for [`History::search`]. All set fields must match (AND).
#[derive(Debug, Clone, Default)]
pub struct SearchQuery<'a> {
    pub text: Option<&'a str>,
    pub directory: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub exit_status: Option<i32>,
    /// Inclusive [from, to] unix-timestamp range.
    pub date_range: Option<(u64, u64)>,
    pub favorites_only: bool,
    /// If false (the default), private entries are excluded from results.
    pub include_private: bool,
}

#[derive(Default)]
pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    pub fn new() -> Self {
        History::default()
    }

    pub fn push(&mut self, entry: HistoryEntry) {
        self.entries.push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record the exit status of the most recently pushed entry (called
    /// once the command actually finishes, since it isn't known at push time).
    pub fn record_exit_status(&mut self, status: i32) {
        if let Some(last) = self.entries.last_mut() {
            last.exit_status = Some(status);
        }
    }

    /// Remove exact-duplicate consecutive commands, keeping the most recent
    /// occurrence's timestamp — "History deduplication".
    pub fn deduplicate(&mut self) {
        let mut seen_last_index = std::collections::HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            seen_last_index.insert(e.command.clone(), i);
        }
        let mut keep: Vec<bool> = vec![false; self.entries.len()];
        for &i in seen_last_index.values() {
            keep[i] = true;
        }
        let mut i = 0;
        self.entries.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }

    pub fn clear_all(&mut self) {
        self.entries.clear();
    }

    /// Remove entries matching `predicate` — "Clear selected history".
    pub fn clear_where(&mut self, predicate: impl Fn(&HistoryEntry) -> bool) {
        self.entries.retain(|e| !predicate(e));
    }

    pub fn search(&self, query: &SearchQuery) -> Vec<&HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| query.include_private || !e.private)
            .filter(|e| query.text.map(|t| e.command.contains(t)).unwrap_or(true))
            .filter(|e| query.directory.map(|d| e.directory == d).unwrap_or(true))
            .filter(|e| query.profile.map(|p| e.profile == p).unwrap_or(true))
            .filter(|e| query.exit_status.map(|s| e.exit_status == Some(s)).unwrap_or(true))
            .filter(|e| query.date_range.map(|(from, to)| e.timestamp >= from && e.timestamp <= to).unwrap_or(true))
            .filter(|e| !query.favorites_only || e.favorite)
            .collect()
    }

    pub fn all(&self) -> &[HistoryEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_by_text() {
        let mut h = History::new();
        h.push(HistoryEntry::now("git status", "/repo", "dev"));
        h.push(HistoryEntry::now("ls -la", "/repo", "dev"));
        let results = h.search(&SearchQuery { text: Some("git"), ..Default::default() });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, "git status");
    }

    #[test]
    fn search_by_directory_and_profile() {
        let mut h = History::new();
        h.push(HistoryEntry::now("ls", "/a", "dev"));
        h.push(HistoryEntry::now("ls", "/b", "admin"));
        let results = h.search(&SearchQuery { directory: Some("/a"), ..Default::default() });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].directory, "/a");
    }

    #[test]
    fn search_by_exit_status() {
        let mut h = History::new();
        h.push(HistoryEntry::now("false", "/a", "dev"));
        h.record_exit_status(1);
        h.push(HistoryEntry::now("true", "/a", "dev"));
        h.record_exit_status(0);
        let failed = h.search(&SearchQuery { exit_status: Some(1), ..Default::default() });
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].command, "false");
    }

    #[test]
    fn private_entries_excluded_by_default() {
        let mut h = History::new();
        let mut secret = HistoryEntry::now("export TOKEN=xyz", "/a", "dev");
        secret.private = true;
        h.push(secret);
        h.push(HistoryEntry::now("ls", "/a", "dev"));
        let results = h.search(&SearchQuery::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, "ls");
    }

    #[test]
    fn favorites_only_filter() {
        let mut h = History::new();
        h.push(HistoryEntry::now("ls", "/a", "dev"));
        let mut fav = HistoryEntry::now("docker compose up", "/a", "dev");
        fav.favorite = true;
        h.push(fav);
        let results = h.search(&SearchQuery { favorites_only: true, ..Default::default() });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, "docker compose up");
    }

    #[test]
    fn deduplicate_keeps_most_recent_occurrence() {
        let mut h = History::new();
        h.push(HistoryEntry::now("ls", "/a", "dev"));
        h.push(HistoryEntry::now("pwd", "/a", "dev"));
        h.push(HistoryEntry::now("ls", "/b", "dev"));
        h.deduplicate();
        assert_eq!(h.len(), 2);
        let commands: Vec<&str> = h.all().iter().map(|e| e.command.as_str()).collect();
        assert!(commands.contains(&"pwd"));
        assert!(commands.contains(&"ls"));
        // the surviving "ls" should be the one run in /b (most recent)
        assert_eq!(h.all().iter().find(|e| e.command == "ls").unwrap().directory, "/b");
    }

    #[test]
    fn clear_where_removes_matching_entries() {
        let mut h = History::new();
        h.push(HistoryEntry::now("rm -rf /tmp/x", "/a", "dev"));
        h.push(HistoryEntry::now("ls", "/a", "dev"));
        h.clear_where(|e| e.command.starts_with("rm"));
        assert_eq!(h.len(), 1);
        assert_eq!(h.all()[0].command, "ls");
    }

    #[test]
    fn date_range_filter() {
        let mut h = History::new();
        let mut old = HistoryEntry::now("old-cmd", "/a", "dev");
        old.timestamp = 100;
        h.push(old);
        let mut recent = HistoryEntry::now("recent-cmd", "/a", "dev");
        recent.timestamp = 200;
        h.push(recent);
        let results = h.search(&SearchQuery { date_range: Some((150, 300)), ..Default::default() });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, "recent-cmd");
    }
}

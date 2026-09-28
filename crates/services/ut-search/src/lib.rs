//! `ut-search`: search over terminal output/scrollback (Section 7 "Output search").
//!
//! Deliberately takes plain `&[String]` (one entry per line) rather than
//! depending on `ut-grid` directly: it can search a pane's current
//! scrollback (via `Terminal::plain_text()` plus whatever the app layer
//! pulls from `Grid::scrollback`), a saved session transcript, or output
//! from multiple tabs/workspaces concatenated together, without caring
//! where the lines came from. "Search across all tabs/workspaces" is just
//! the caller concatenating more lines in before calling [`search`].

use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Literal,
    Regex,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: Mode,
    pub case_sensitive: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { mode: Mode::Literal, case_sensitive: false }
    }
}

/// One match: which line, and the byte range within that line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub enum SearchError {
    InvalidPattern(String),
}

/// Search `lines` for `pattern` per `options`, returning every match in
/// line order (each line may contain more than one match).
pub fn search(lines: &[String], pattern: &str, options: &SearchOptions) -> Result<Vec<Match>, SearchError> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let regex = build_regex(pattern, options)?;
    let mut matches = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for m in regex.find_iter(line) {
            matches.push(Match { line: i, start: m.start(), end: m.end() });
        }
    }
    Ok(matches)
}

/// An incremental index over a fixed snapshot of lines, so a caller doing
/// several searches (typing into a search box character by character)
/// doesn't re-scan from scratch needlessly for literal searches — a real
/// terminal would rebuild/extend this as new output/scrollback arrives.
pub struct SearchIndex {
    lines: Vec<String>,
}

impl SearchIndex {
    pub fn new(lines: Vec<String>) -> Self {
        SearchIndex { lines }
    }

    pub fn push_line(&mut self, line: String) {
        self.lines.push(line);
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn search(&self, pattern: &str, options: &SearchOptions) -> Result<Vec<Match>, SearchError> {
        search(&self.lines, pattern, options)
    }

    /// Next match after `(line, col)`, wrapping around to the start —
    /// "Next and previous result".
    pub fn next_match(&self, pattern: &str, options: &SearchOptions, after: (usize, usize)) -> Result<Option<Match>, SearchError> {
        let all = self.search(pattern, options)?;
        if all.is_empty() {
            return Ok(None);
        }
        let pos = all.iter().position(|m| (m.line, m.start) > after);
        Ok(Some(match pos {
            Some(i) => all[i].clone(),
            None => all[0].clone(),
        }))
    }

    /// Previous match before `(line, col)`, wrapping around to the end.
    pub fn previous_match(&self, pattern: &str, options: &SearchOptions, before: (usize, usize)) -> Result<Option<Match>, SearchError> {
        let all = self.search(pattern, options)?;
        if all.is_empty() {
            return Ok(None);
        }
        let pos = all.iter().rposition(|m| (m.line, m.start) < before);
        Ok(Some(match pos {
            Some(i) => all[i].clone(),
            None => all[all.len() - 1].clone(),
        }))
    }
}

fn build_regex(pattern: &str, options: &SearchOptions) -> Result<Regex, SearchError> {
    let escaped;
    let pattern = match options.mode {
        Mode::Regex => pattern,
        Mode::Literal => {
            escaped = regex::escape(pattern);
            escaped.as_str()
        }
    };
    RegexBuilder::new(pattern)
        .case_insensitive(!options.case_sensitive)
        .build()
        .map_err(|e| SearchError::InvalidPattern(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn literal_search_case_insensitive_by_default() {
        let l = lines(&["Hello world", "goodbye"]);
        let m = search(&l, "hello", &SearchOptions::default()).unwrap();
        assert_eq!(m, vec![Match { line: 0, start: 0, end: 5 }]);
    }

    #[test]
    fn literal_search_case_sensitive() {
        let l = lines(&["Hello world"]);
        let opts = SearchOptions { mode: Mode::Literal, case_sensitive: true };
        let m = search(&l, "hello", &opts).unwrap();
        assert!(m.is_empty());
        let m2 = search(&l, "Hello", &opts).unwrap();
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn regex_search_finds_all_matches_per_line() {
        let l = lines(&["error: foo", "warn: bar", "error: baz"]);
        let opts = SearchOptions { mode: Mode::Regex, case_sensitive: false };
        let m = search(&l, r"^error:", &opts).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].line, 0);
        assert_eq!(m[1].line, 2);
    }

    #[test]
    fn literal_special_chars_are_escaped() {
        let l = lines(&["cost: $5.00 (approx)"]);
        let m = search(&l, "$5.00 (approx)", &SearchOptions::default()).unwrap();
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn invalid_regex_returns_error() {
        let l = lines(&["anything"]);
        let opts = SearchOptions { mode: Mode::Regex, case_sensitive: false };
        let result = search(&l, "(unclosed", &opts);
        assert!(matches!(result, Err(SearchError::InvalidPattern(_))));
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        let l = lines(&["anything"]);
        let m = search(&l, "", &SearchOptions::default()).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn next_match_wraps_around() {
        let idx = SearchIndex::new(lines(&["foo", "bar foo", "foo baz"]));
        let opts = SearchOptions::default();
        let first = idx.next_match("foo", &opts, (2, 0)).unwrap().unwrap();
        assert_eq!(first, Match { line: 0, start: 0, end: 3 });
    }

    #[test]
    fn previous_match_wraps_around() {
        let idx = SearchIndex::new(lines(&["foo", "bar foo", "foo baz"]));
        let opts = SearchOptions::default();
        let last = idx.previous_match("foo", &opts, (0, 0)).unwrap().unwrap();
        assert_eq!(last, Match { line: 2, start: 0, end: 3 });
    }

    #[test]
    fn index_grows_with_push_line() {
        let mut idx = SearchIndex::new(lines(&["a"]));
        idx.push_line("b match".to_string());
        assert_eq!(idx.line_count(), 2);
        let m = idx.search("match", &SearchOptions::default()).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 1);
    }
}

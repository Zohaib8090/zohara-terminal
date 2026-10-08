//! `ut-palette`: command palette / launcher (Section 20).
//!
//! Toolkit-agnostic, like `ut-ui`: [`Palette`] owns a flat list of
//! [`PaletteItem`]s from whatever the app wants to make searchable
//! (registered commands, open tabs/panes/workspaces, saved SSH hosts,
//! saved workflows, files, settings — anything with a label), plus
//! recently-used tracking, favorites, and aliases. [`PaletteState`] is the
//! small bit of UI state (current query, selected index) a front end
//! drives with keystrokes; neither type knows about ratatui or egui.

mod fuzzy;

pub use fuzzy::{matches, score};

use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Command,
    Action,
    Tab,
    Pane,
    Workspace,
    SshHost,
    Workflow,
    File,
    Application,
    Setting,
    Documentation,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub label: String,
    pub category: Category,
    pub subtitle: Option<String>,
    /// Extra search terms beyond the label (e.g. a command's other names).
    pub keywords: Vec<String>,
}

impl PaletteItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, category: Category) -> Self {
        PaletteItem { id: id.into(), label: label.into(), category, subtitle: None, keywords: Vec::new() }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn with_keyword(mut self, keyword: impl Into<String>) -> Self {
        self.keywords.push(keyword.into());
        self
    }
}

const RECENT_LIMIT: usize = 20;
/// Boost given to items that matched via an alias rather than their label —
/// an alias is an intentional shortcut, so it should win ties.
const ALIAS_MATCH_BONUS: i64 = 1000;
const FAVORITE_BONUS: i64 = 50;
/// Recency bonus: the most-recently-used item gets this; older ones taper off.
const RECENCY_BASE_BONUS: i64 = 30;

/// The searchable registry: items plus recent/favorite/alias state.
#[derive(Default)]
pub struct Palette {
    items: Vec<PaletteItem>,
    recent: VecDeque<String>,
    favorites: HashSet<String>,
    /// alias text (lowercased) -> item id.
    aliases: HashMap<String, String>,
}

impl Palette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_items(&mut self, items: Vec<PaletteItem>) {
        self.items = items;
    }

    pub fn add_item(&mut self, item: PaletteItem) {
        self.items.push(item);
    }

    pub fn item(&self, id: &str) -> Option<&PaletteItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Register a custom alias for an item ("Command aliases") — typing
    /// the alias finds the item even if it matches nothing in its label or keywords.
    pub fn add_alias(&mut self, alias: impl Into<String>, item_id: impl Into<String>) {
        self.aliases.insert(alias.into().to_lowercase(), item_id.into());
    }

    pub fn toggle_favorite(&mut self, id: &str) {
        if !self.favorites.remove(id) {
            self.favorites.insert(id.to_string());
        }
    }

    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.contains(id)
    }

    /// Record that `id` was just used ("Recently used actions") — moves it
    /// to the front of the MRU list, trimming to [`RECENT_LIMIT`].
    pub fn record_used(&mut self, id: &str) {
        self.recent.retain(|existing| existing != id);
        self.recent.push_front(id.to_string());
        self.recent.truncate(RECENT_LIMIT);
    }

    fn recency_bonus(&self, id: &str) -> i64 {
        self.recent.iter().position(|r| r == id).map(|pos| (RECENCY_BASE_BONUS - pos as i64).max(0)).unwrap_or(0)
    }

    /// Search items against `query`. An empty query returns everything,
    /// most-recently-used first, then favorites, then registration order —
    /// "Recently used actions" / "Favorite actions" as the default view
    /// before the user types anything.
    pub fn search(&self, query: &str) -> Vec<&PaletteItem> {
        let query_trimmed = query.trim();

        if query_trimmed.is_empty() {
            let mut items: Vec<&PaletteItem> = self.items.iter().collect();
            items.sort_by_key(|item| {
                let recency = self.recency_bonus(&item.id);
                let favorite = if self.is_favorite(&item.id) { 1 } else { 0 };
                std::cmp::Reverse((recency, favorite))
            });
            return items;
        }

        let alias_target = self.aliases.get(&query_trimmed.to_lowercase()).cloned();

        let mut scored: Vec<(i64, &PaletteItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let mut best = score(query_trimmed, &item.label);
                for kw in &item.keywords {
                    if let Some(s) = score(query_trimmed, kw) {
                        best = Some(best.map_or(s, |b| b.max(s)));
                    }
                }
                if alias_target.as_deref() == Some(item.id.as_str()) {
                    best = Some(best.unwrap_or(0) + ALIAS_MATCH_BONUS);
                }
                let base = best?;
                let favorite_bonus = if self.is_favorite(&item.id) { FAVORITE_BONUS } else { 0 };
                let recency = self.recency_bonus(&item.id);
                Some((base + favorite_bonus + recency, item))
            })
            .collect();

        // Highest score first; ties broken alphabetically (ascending) so
        // equally-ranked results land in a stable, predictable order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

/// UI-facing state: the current query and which result is highlighted.
/// Drive this from keystrokes; a front end renders `visible_items()` and
/// highlights `selected_index()`.
#[derive(Default)]
pub struct PaletteState {
    query: String,
    selected: usize,
}

impl PaletteState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.selected = 0;
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.selected = 0;
    }

    /// Move the selection down, clamped to the last visible item.
    pub fn move_down(&mut self, visible_count: usize) {
        if visible_count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1).min(visible_count - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Run the current query against `palette` and return the matching
    /// items in rank order — what a front end should render this frame.
    pub fn visible_items<'a>(&self, palette: &'a Palette) -> Vec<&'a PaletteItem> {
        palette.search(&self.query)
    }

    /// The currently-highlighted item for this query, if any (accounts for
    /// `selected` being clamped if the result list shrank since it was set).
    pub fn selected_item<'a>(&self, palette: &'a Palette) -> Option<&'a PaletteItem> {
        let items = self.visible_items(palette);
        items.get(self.selected.min(items.len().saturating_sub(1))).copied()
    }

    /// Confirm the current selection: records it as recently-used and
    /// returns its id for the caller to actually dispatch.
    pub fn confirm(&mut self, palette: &mut Palette) -> Option<String> {
        let id = self.selected_item(palette)?.id.clone();
        palette.record_used(&id);
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_palette() -> Palette {
        let mut p = Palette::new();
        p.set_items(vec![
            PaletteItem::new("new-ssh", "New SSH Session", Category::SshHost).with_keyword("connect"),
            PaletteItem::new("split-h", "Split Pane Horizontally", Category::Action),
            PaletteItem::new("split-v", "Split Pane Vertically", Category::Action),
            PaletteItem::new("settings", "Open Settings", Category::Setting),
            PaletteItem::new("refresh-db", "Refresh Package Databases", Category::Command),
        ]);
        p
    }

    #[test]
    fn empty_query_returns_everything() {
        let p = sample_palette();
        assert_eq!(p.search("").len(), 5);
    }

    #[test]
    fn search_filters_to_matching_items() {
        let p = sample_palette();
        let results = p.search("split");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|i| i.id.starts_with("split")));
    }

    #[test]
    fn search_matches_keywords_not_just_label() {
        let p = sample_palette();
        let results = p.search("connect");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "new-ssh");
    }

    #[test]
    fn no_match_returns_empty() {
        let p = sample_palette();
        assert!(p.search("zzzznomatch").is_empty());
    }

    #[test]
    fn alias_finds_item_even_without_label_match() {
        let mut p = sample_palette();
        p.add_alias("ns", "new-ssh");
        let results = p.search("ns");
        assert_eq!(results[0].id, "new-ssh");
    }

    #[test]
    fn alias_is_case_insensitive() {
        let mut p = sample_palette();
        p.add_alias("NS", "new-ssh");
        let results = p.search("ns");
        assert_eq!(results[0].id, "new-ssh");
    }

    #[test]
    fn recently_used_items_come_first_on_empty_query() {
        let mut p = sample_palette();
        p.record_used("settings");
        let results = p.search("");
        assert_eq!(results[0].id, "settings");
    }

    #[test]
    fn most_recent_use_wins_over_older_use() {
        let mut p = sample_palette();
        p.record_used("settings");
        p.record_used("refresh-db");
        let results = p.search("");
        assert_eq!(results[0].id, "refresh-db");
        assert_eq!(results[1].id, "settings");
    }

    #[test]
    fn favorites_rank_above_non_favorites_on_empty_query() {
        let mut p = sample_palette();
        p.toggle_favorite("split-v");
        let results = p.search("");
        assert_eq!(results[0].id, "split-v");
    }

    #[test]
    fn toggle_favorite_is_reversible() {
        let mut p = sample_palette();
        p.toggle_favorite("split-v");
        assert!(p.is_favorite("split-v"));
        p.toggle_favorite("split-v");
        assert!(!p.is_favorite("split-v"));
    }

    #[test]
    fn favorite_boosts_ranking_in_a_text_search_too() {
        let mut p = sample_palette();
        // "split" matches both split items roughly equally; favoriting one should win.
        p.toggle_favorite("split-v");
        let results = p.search("split");
        assert_eq!(results[0].id, "split-v");
    }

    #[test]
    fn state_tracks_query_and_resets_selection_on_change() {
        let mut state = PaletteState::new();
        state.push_char('s');
        state.push_char('s');
        state.move_down(5);
        assert_eq!(state.selected_index(), 1);
        state.push_char('h');
        assert_eq!(state.selected_index(), 0); // typing resets selection
        assert_eq!(state.query(), "ssh");
    }

    #[test]
    fn backspace_removes_last_character() {
        let mut state = PaletteState::new();
        state.set_query("ssh");
        state.backspace();
        assert_eq!(state.query(), "ss");
    }

    #[test]
    fn move_down_clamps_to_last_visible_item() {
        let mut state = PaletteState::new();
        state.move_down(3);
        state.move_down(3);
        state.move_down(3);
        state.move_down(3);
        assert_eq!(state.selected_index(), 2);
    }

    #[test]
    fn move_up_clamps_at_zero() {
        let mut state = PaletteState::new();
        state.move_up();
        assert_eq!(state.selected_index(), 0);
    }

    #[test]
    fn selected_item_reflects_current_query_and_index() {
        let p = sample_palette();
        let mut state = PaletteState::new();
        state.set_query("split");
        state.move_down(2);
        let item = state.selected_item(&p).unwrap();
        assert_eq!(item.id, "split-v");
    }

    #[test]
    fn confirm_records_recent_use_and_returns_id() {
        let mut p = sample_palette();
        let mut state = PaletteState::new();
        state.set_query("settings");
        let id = state.confirm(&mut p).unwrap();
        assert_eq!(id, "settings");
        assert!(p.search("").iter().position(|i| i.id == "settings") == Some(0));
    }

    #[test]
    fn confirm_with_no_results_returns_none() {
        let mut p = sample_palette();
        let mut state = PaletteState::new();
        state.set_query("zzzznomatch");
        assert_eq!(state.confirm(&mut p), None);
    }

    #[test]
    fn selection_clamps_when_query_shrinks_result_set() {
        let p = sample_palette();
        let mut state = PaletteState::new();
        state.set_query("split");
        state.move_down(2); // select index 1 (2 split results)
        state.set_query("split h"); // narrows to 1 result ("Split Pane Horizontally")
        // query change resets selection to 0, which should still resolve.
        let item = state.selected_item(&p);
        assert!(item.is_some());
    }

    #[test]
    fn clear_resets_query_and_selection() {
        let mut state = PaletteState::new();
        state.set_query("split");
        state.move_down(2);
        state.clear();
        assert_eq!(state.query(), "");
        assert_eq!(state.selected_index(), 0);
    }
}

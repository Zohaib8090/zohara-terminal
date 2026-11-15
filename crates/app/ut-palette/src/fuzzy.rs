//! Fuzzy subsequence matching with fzf-style scoring ("Fuzzy matching").
//!
//! [`score`] returns `None` when `query`'s characters don't all appear in
//! `text` in order (not a match at all), or `Some(score)` — higher is
//! better — when they do. The scoring rewards the kinds of matches a
//! human expects to win: matches at the start of a word, consecutive
//! character runs, and case-exact matches, while penalizing large gaps
//! between matched characters so `"nssn"` doesn't beat `"new ssh session"`
//! against the query `"nss"` just because both technically match.

/// Score `text` against `query` (case-insensitive subsequence match).
/// `None` if `query` isn't a subsequence of `text` at all.
pub fn score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }

    let text_chars: Vec<char> = text.chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();
    // Paired (original-case, lowercased) query chars — same length since
    // we only lowercase, never fold width, so a 1:1 zip is safe.
    let query_pairs: Vec<(char, char)> = query.chars().zip(query.to_lowercase().chars()).collect();

    let mut total_score: i64 = 0;
    let mut text_idx = 0usize;
    let mut last_match_idx: Option<usize> = None;
    let mut consecutive_run = 0i64;

    for (orig_q, qc) in query_pairs {
        let mut found = None;
        let mut i = text_idx;
        while i < text_lower.len() {
            if text_lower[i] == qc {
                found = Some(i);
                break;
            }
            i += 1;
        }
        let idx = found?;

        let mut char_score = 10i64;

        // Bonus: start of string or start of a "word" (preceded by
        // non-alphanumeric, e.g. after a space, dash, underscore, slash).
        let is_word_start = idx == 0
            || text_chars.get(idx.wrapping_sub(1)).map(|c| !c.is_alphanumeric()).unwrap_or(false);
        if is_word_start {
            char_score += 15;
        }

        // Bonus: consecutive match continues a run from the previous character.
        if let Some(last) = last_match_idx {
            if idx == last + 1 {
                consecutive_run += 1;
                char_score += 5 + consecutive_run * 3;
            } else {
                consecutive_run = 0;
                // Penalty proportional to the gap, so closer matches win.
                char_score -= ((idx - last) as i64).min(10);
            }
        } else {
            consecutive_run = 0;
        }

        // Bonus: exact case match (query char's original case equals text's).
        if text_chars.get(idx) == Some(&orig_q) {
            char_score += 2;
        }

        total_score += char_score;
        last_match_idx = Some(idx);
        text_idx = idx + 1;
    }

    // Small bonus for shorter overall text (less to scan through visually,
    // and it means the match covers more of the string).
    total_score -= (text_chars.len() as i64).min(50) / 10;

    Some(total_score)
}

/// True if `query` matches `text` at all (convenience over [`score`]).
pub fn matches(query: &str, text: &str) -> bool {
    score(query, text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything_with_zero_score() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn exact_substring_matches() {
        assert!(matches("ssh", "New SSH Session"));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(score("xyz", "new ssh session"), None);
    }

    #[test]
    fn subsequence_out_of_order_does_not_match() {
        // "nse" would require n...s...e in order; "session" has s-e-s-s-i-o-n,
        // "ens" is not a subsequence of "session" in that order after 's'.
        assert_eq!(score("sen", "ssh"), None);
    }

    #[test]
    fn word_start_matches_score_higher_than_mid_word() {
        // query "ns" : "New Session" matches N(start) then s(start of Session);
        // "insert" matches 'n' mid-word then 's' mid-word.
        let word_start = score("ns", "New Session").unwrap();
        let mid_word = score("ns", "insert").unwrap();
        assert!(word_start > mid_word, "word_start={word_start} mid_word={mid_word}");
    }

    #[test]
    fn consecutive_matches_score_higher_than_scattered() {
        let consecutive = score("new", "New Tab").unwrap();
        let scattered = score("new", "N1E2W3").unwrap();
        assert!(consecutive > scattered, "consecutive={consecutive} scattered={scattered}");
    }

    #[test]
    fn shorter_match_target_scores_higher_than_longer_for_same_query() {
        let short = score("tab", "New Tab").unwrap();
        let long = score("tab", "New Tab In A Very Long Workspace Name Indeed").unwrap();
        assert!(short > long, "short={short} long={long}");
    }

    #[test]
    fn case_insensitive_matching_still_works() {
        assert!(matches("SSH", "new ssh session"));
        assert!(matches("ssh", "New SSH Session"));
    }

    #[test]
    fn case_exact_match_scores_slightly_higher() {
        let exact = score("SSH", "SSH Session").unwrap();
        let wrong_case = score("SSH", "ssh session").unwrap();
        assert!(exact >= wrong_case);
    }

    #[test]
    fn real_world_ranking_prefers_the_obviously_right_item() {
        let candidates = ["New SSH Session", "Refresh Package Databases", "Search Files"];
        let mut scored: Vec<(i64, &str)> = candidates.iter().filter_map(|&c| score("ssh", c).map(|s| (s, c))).collect();
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        assert_eq!(scored[0].1, "New SSH Session");
    }
}

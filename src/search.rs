//! Searching entry names in the current folder with `/`, with no dependency on the UI.
//!
//! A match is a substring of the name. Matching ignores case unless the pattern has an uppercase
//! letter ("smartcase", as in vim).

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// Byte ranges of every non-overlapping match of `pattern` in `name`, from left to right. Empty if
/// the pattern is empty or does not occur.
pub fn match_ranges(name: &str, pattern: &str) -> Vec<Range<usize>> {
    let pattern: Vec<char> = pattern.chars().collect();
    if pattern.is_empty() {
        return Vec::new();
    }
    let case_sensitive = pattern.iter().any(|c| c.is_uppercase());
    let same =
        |a: char, b: char| a == b || !case_sensitive && a.to_lowercase().eq(b.to_lowercase());
    // Compared character by character, so the ranges stay on the original name's boundaries even
    // where lowercasing changes the length in bytes.
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    let mut ranges = Vec::new();
    let mut start = 0;
    while start + pattern.len() <= chars.len() {
        let matched = pattern
            .iter()
            .enumerate()
            .all(|(offset, &p)| same(chars[start + offset].1, p));
        if matched {
            let end = start + pattern.len();
            let end_byte = chars.get(end).map_or(name.len(), |&(byte, _)| byte);
            ranges.push(chars[start].0..end_byte);
            start = end;
        } else {
            start += 1;
        }
    }
    ranges
}

pub fn matches(name: &str, pattern: &str) -> bool {
    !match_ranges(name, pattern).is_empty()
}

/// The next row in `0..count` for which `is_match` holds, starting at `from` and going in
/// `direction`, wrapping around the ends. With `include_from`, `from` itself is the first
/// candidate; without it, `from` is the last one (after a full turn). Returns the row and whether
/// the search wrapped around.
pub fn next_match(
    count: usize,
    from: usize,
    direction: Direction,
    include_from: bool,
    is_match: impl Fn(usize) -> bool,
) -> Option<(usize, bool)> {
    if count == 0 {
        return None;
    }
    let from = from.min(count - 1);
    let steps = if include_from { 0..count } else { 1..count + 1 };
    steps
        .map(|step| match direction {
            Direction::Forward => ((from + step) % count, from + step >= count),
            Direction::Backward => ((from + count - step % count) % count, step > from),
        })
        .find(|&(row, _)| is_match(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ignoring_case_unless_the_pattern_has_uppercase() {
        assert_eq!(match_ranges("alphabet.md", "al"), vec![0..2]);
        assert_eq!(match_ranges("Alpha", "al"), vec![0..2]);
        assert_eq!(match_ranges("Alpha", "Al"), vec![0..2]);
        assert_eq!(match_ranges("alpha", "Al"), Vec::<Range<usize>>::new());
        assert_eq!(match_ranges("Gamma.txt", "G"), vec![0..1]);
        assert_eq!(match_ranges("bigger", "G"), Vec::<Range<usize>>::new());
        assert_eq!(match_ranges("bigger", "g"), vec![2..3, 3..4]);
        assert!(match_ranges("alpha", "").is_empty());
        assert!(match_ranges("al", "alpha").is_empty());
    }

    #[test]
    fn finds_every_match_without_overlap() {
        assert_eq!(match_ranges("banana", "ana"), vec![1..4]);
        assert_eq!(match_ranges("aaaa", "aa"), vec![0..2, 2..4]);
        assert_eq!(
            match_ranges("a.txt.txt", ".TXT"),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(match_ranges("a.txt.txt", ".txt"), vec![1..5, 5..9]);
    }

    #[test]
    fn ranges_follow_the_original_bytes() {
        // `Ñ` and `ú` take two bytes each.
        assert_eq!(match_ranges("Ñandú.txt", "ñan"), vec![0..4]);
        assert_eq!(match_ranges("Ñandú.txt", "dú"), vec![4..7]);
        assert_eq!(match_ranges("straße", "SSE"), Vec::<Range<usize>>::new());
        assert_eq!(match_ranges("straße", "ße"), vec![4..7]);
        // `İ` lowercases to two characters: it only matches itself.
        assert_eq!(match_ranges("İstanbul", "stan"), vec![2..6]);
        assert!(matches("日本語.md", "本"));
    }

    #[test]
    fn steps_forward_and_backward_with_wrapping() {
        // Matching rows: 1 and 3 of 5.
        let is_match = |row: usize| row == 1 || row == 3;
        let next = |from, direction, include| next_match(5, from, direction, include, is_match);
        assert_eq!(next(0, Direction::Forward, true), Some((1, false)));
        assert_eq!(next(1, Direction::Forward, true), Some((1, false)));
        assert_eq!(next(1, Direction::Forward, false), Some((3, false)));
        assert_eq!(next(3, Direction::Forward, false), Some((1, true)));
        assert_eq!(next(4, Direction::Forward, true), Some((1, true)));
        assert_eq!(next(3, Direction::Backward, false), Some((1, false)));
        assert_eq!(next(1, Direction::Backward, false), Some((3, true)));
        assert_eq!(next(0, Direction::Backward, true), Some((3, true)));
    }

    #[test]
    fn a_single_match_is_found_again_after_a_full_turn() {
        assert_eq!(
            next_match(4, 2, Direction::Forward, false, |row| row == 2),
            Some((2, true))
        );
        assert_eq!(
            next_match(4, 2, Direction::Backward, false, |row| row == 2),
            Some((2, true))
        );
    }

    #[test]
    fn nothing_to_find() {
        assert_eq!(next_match(0, 0, Direction::Forward, true, |_| true), None);
        assert_eq!(next_match(3, 1, Direction::Forward, true, |_| false), None);
        // A cursor past the end (the list shrank) starts from the last row.
        assert_eq!(
            next_match(3, 9, Direction::Forward, true, |row| row == 2),
            Some((2, false))
        );
    }
}

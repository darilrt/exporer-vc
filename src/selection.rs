//! Marked entries, with no dependency on the UI.
//!
//! Marks are full paths, so they survive moving to another folder and can gather entries from
//! several folders for one operation.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Marks {
    /// Ordered, so operations go through the entries in a stable order.
    paths: BTreeSet<PathBuf>,
}

impl Marks {
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// Marks `path` if it was not marked and unmarks it otherwise. Returns whether it ended up
    /// marked.
    pub fn toggle(&mut self, path: PathBuf) -> bool {
        if self.paths.remove(&path) {
            false
        } else {
            self.paths.insert(path);
            true
        }
    }

    pub fn extend(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.paths.extend(paths);
    }

    pub fn clear(&mut self) {
        self.paths.clear();
    }

    /// Drops the marks of entries of `dir` that are not in `names`: they were deleted or renamed
    /// since they were marked. Marks in other folders are kept.
    pub fn retain_listed(&mut self, dir: &Path, names: &HashSet<&OsStr>) {
        self.paths.retain(|path| {
            path.parent() != Some(dir) || path.file_name().is_some_and(|name| names.contains(name))
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().map(PathBuf::as_path)
    }
}

/// What an operation acts on: the marks if there are any, otherwise the entry under the cursor
/// (`None` on the `..` row, which is never a target).
pub fn targets(marks: &Marks, cursor: Option<PathBuf>) -> Vec<PathBuf> {
    if marks.is_empty() {
        cursor.into_iter().collect()
    } else {
        marks.iter().map(Path::to_path_buf).collect()
    }
}

/// Rows covered by a visual selection that started at `anchor`, whichever side the cursor is on.
pub fn visual_rows(anchor: usize, cursor: usize) -> RangeInclusive<usize> {
    anchor.min(cursor)..=anchor.max(cursor)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn toggles_marks() {
        let mut marks = Marks::default();
        assert!(marks.toggle("/a/x".into()));
        assert!(marks.contains(Path::new("/a/x")));
        assert!(!marks.toggle("/a/x".into()));
        assert!(marks.is_empty());
    }

    #[test]
    fn targets_are_the_marks_or_the_cursor() {
        let mut marks = Marks::default();
        assert_eq!(targets(&marks, None), Vec::<PathBuf>::new());
        assert_eq!(
            targets(&marks, Some("/a/c".into())),
            vec![PathBuf::from("/a/c")]
        );
        marks.extend([PathBuf::from("/b/z"), PathBuf::from("/a/y")]);
        assert_eq!(
            targets(&marks, Some("/a/c".into())),
            vec![PathBuf::from("/a/y"), PathBuf::from("/b/z")]
        );
    }

    #[test]
    fn visual_rows_go_both_ways() {
        assert_eq!(visual_rows(2, 5), 2..=5);
        assert_eq!(visual_rows(5, 2), 2..=5);
        assert_eq!(visual_rows(3, 3), 3..=3);
    }

    #[test]
    fn drops_marks_of_missing_entries_only_in_the_listed_folder() {
        let mut marks = Marks::default();
        marks.extend([
            PathBuf::from("/a/kept"),
            PathBuf::from("/a/gone"),
            PathBuf::from("/b/gone"),
            PathBuf::from("/a/sub/gone"),
        ]);
        let kept = OsString::from("kept");
        let names: HashSet<&OsStr> = [kept.as_os_str()].into_iter().collect();
        marks.retain_listed(Path::new("/a"), &names);
        let left: Vec<_> = marks.iter().collect();
        assert_eq!(
            left,
            [
                Path::new("/a/kept"),
                Path::new("/a/sub/gone"),
                Path::new("/b/gone")
            ]
        );
    }
}

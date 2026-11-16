//! Completing a typed path with `Tab`, as a shell does, with no dependency on the UI.
//!
//! Only the last segment is completed, against the entries of the folder the rest names. Matching
//! ignores case unless the typed segment has an uppercase letter ("smartcase"); hidden entries
//! only show when the segment starts with `.`.

use std::fs;
use std::io;
use std::path::Path;

use crate::listing;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub is_dir: bool,
}

impl Candidate {
    /// The text it puts in the field: folders end in `/`, so the next segment can follow.
    pub fn text(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Splits the typed text into the folder part, kept as typed (`~/Doc` → `~/`), and the segment
/// being completed (`Doc`).
pub fn split(text: &str) -> (&str, &str) {
    match text.rfind('/') {
        Some(slash) => text.split_at(slash + 1),
        None => ("", text),
    }
}

/// The entries of `dir` that `prefix` can complete to, folders first and then by name. Entries
/// whose name is not valid UTF-8 are left out: they could not be typed anyway.
pub fn candidates(dir: &Path, prefix: &str) -> io::Result<Vec<Candidate>> {
    let entries = fs::read_dir(dir)?.filter_map(|entry| {
        let entry = entry.ok()?;
        let name = entry.file_name().into_string().ok()?;
        // Follows links, so a link to a folder completes with `/`.
        let is_dir = fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir());
        Some(Candidate { name, is_dir })
    });
    Ok(matching(entries, prefix))
}

pub fn matching(entries: impl IntoIterator<Item = Candidate>, prefix: &str) -> Vec<Candidate> {
    let case_sensitive = prefix.chars().any(char::is_uppercase);
    let show_hidden = prefix.starts_with('.');
    let mut found: Vec<Candidate> = entries
        .into_iter()
        .filter(|candidate| show_hidden || !candidate.name.starts_with('.'))
        .filter(|candidate| starts_with(&candidate.name, prefix, case_sensitive))
        .collect();
    found.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| listing::compare_names(&a.name, &b.name))
    });
    found
}

fn starts_with(name: &str, prefix: &str, case_sensitive: bool) -> bool {
    let mut name = name.chars();
    prefix.chars().all(|p| match name.next() {
        Some(c) if case_sensitive => c == p,
        Some(c) => c == p || c.to_lowercase().eq(p.to_lowercase()),
        None => false,
    })
}

/// The longest start shared by every candidate, exactly as written in their names. With a single
/// candidate, its whole text (with `/` for a folder).
pub fn common_prefix(candidates: &[Candidate]) -> String {
    match candidates {
        [] => String::new(),
        [only] => only.text(),
        [first, rest @ ..] => {
            let mut length = first.name.len();
            for other in rest {
                length = first
                    .name
                    .char_indices()
                    .zip(other.name.chars())
                    .find(|((_, a), b)| a != b)
                    .map_or(length.min(other.name.len()), |((index, _), _)| index)
                    .min(length);
            }
            first.name[..length].to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[(&str, bool)]) -> Vec<Candidate> {
        names
            .iter()
            .map(|&(name, is_dir)| Candidate {
                name: name.to_owned(),
                is_dir,
            })
            .collect()
    }

    fn names(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn splits_off_the_last_segment() {
        assert_eq!(split("/home/u/Doc"), ("/home/u/", "Doc"));
        assert_eq!(split("~/"), ("~/", ""));
        assert_eq!(split("Doc"), ("", "Doc"));
        assert_eq!(split("/"), ("/", ""));
    }

    #[test]
    fn matches_with_smartcase_folders_first() {
        let all = entries(&[
            ("documents.txt", false),
            ("Downloads", true),
            ("Documents", true),
            ("desk", false),
            (".dotfile", false),
            ("Music", true),
        ]);
        assert_eq!(
            names(&matching(all.clone(), "do")),
            ["Documents", "Downloads", "documents.txt"]
        );
        assert_eq!(names(&matching(all.clone(), "Do")), ["Documents", "Downloads"]);
        assert_eq!(names(&matching(all.clone(), "")).len(), 5);
        assert_eq!(names(&matching(all.clone(), ".")), [".dotfile"]);
        assert!(matching(all, "zzz").is_empty());
    }

    #[test]
    fn completes_the_shared_start() {
        let one = entries(&[("Documents", true)]);
        assert_eq!(common_prefix(&one), "Documents/");
        let file = entries(&[("notes.txt", false)]);
        assert_eq!(common_prefix(&file), "notes.txt");
        let two = entries(&[("Documents", true), ("Downloads", true)]);
        assert_eq!(common_prefix(&two), "Do");
        let nested = entries(&[("abc", false), ("abcd", false), ("abx", false)]);
        assert_eq!(common_prefix(&nested), "ab");
        let accents = entries(&[("añoA", false), ("añoB", false)]);
        assert_eq!(common_prefix(&accents), "año");
        let differ = entries(&[("Docs", true), ("docs.txt", false)]);
        assert_eq!(common_prefix(&differ), "");
    }

    #[test]
    fn reads_a_real_folder() {
        let dir = std::env::temp_dir().join(format!("exporer-complete-{}", std::process::id()));
        fs::create_dir_all(dir.join("Documents")).unwrap();
        fs::write(dir.join("Doc.txt"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("Documents"), dir.join("DocLink")).unwrap();
        let found = candidates(&dir, "Doc").unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            found,
            entries(&[("DocLink", true), ("Documents", true), ("Doc.txt", false)])
        );
    }
}

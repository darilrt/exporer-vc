//! Reading and formatting folders, with no dependency on the UI.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry, Metadata};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use gpui::SharedString;
use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;

use crate::assets::icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    Image,
    File,
}

impl Kind {
    pub fn icon(self) -> &'static str {
        match self {
            Kind::Dir => icon::FOLDER_FILL,
            Kind::Image => icon::IMAGE,
            Kind::File => icon::FILE,
        }
    }
}

/// A folder entry with its texts already formatted, so drawing a row neither touches the disk
/// nor computes anything.
pub struct Entry {
    pub file_name: OsString,
    pub name: SharedString,
    pub kind: Kind,
    pub size_text: SharedString,
    pub modified_text: SharedString,
    pub permissions_text: SharedString,
    pub type_text: SharedString,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "gif", "heic", "jpeg", "jpg", "png", "tif", "tiff", "webp",
];

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const EMPTY: &str = "—";

/// Reads the folder and returns its entries sorted. Meant to run off the UI thread. Entries that
/// cannot be read are skipped; an error reading the folder itself is returned.
pub fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let tz = TimeZone::system();
    let today = tz.to_datetime(Timestamp::now()).date();
    let mut entries: Vec<Entry> = fs::read_dir(path)?
        .filter_map(Result::ok)
        .map(|item| make_entry(&item, &tz, today))
        .collect();
    sort_entries(&mut entries);
    Ok(entries)
}

fn make_entry(item: &DirEntry, tz: &TimeZone, today: Date) -> Entry {
    let file_name = item.file_name();
    let name = file_name.to_string_lossy().into_owned();
    let is_symlink = item.file_type().is_ok_and(|t| t.is_symlink());
    // One stat per entry: the link is followed only when the entry is one.
    let metadata = if is_symlink {
        fs::metadata(item.path()).ok()
    } else {
        item.metadata().ok()
    };
    let is_dir = metadata.as_ref().is_some_and(Metadata::is_dir);
    let kind = kind_of(&name, is_dir);

    let size_text = match &metadata {
        Some(m) if !is_dir => format_size(m.len()),
        _ => EMPTY.to_owned(),
    };
    let modified_text = metadata
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| Timestamp::try_from(t).ok())
        .map(|ts| format_modified(tz.to_datetime(ts), today))
        .unwrap_or_else(|| EMPTY.to_owned());
    let permissions_text = match &metadata {
        Some(m) => {
            let type_char = if is_symlink {
                'l'
            } else if is_dir {
                'd'
            } else {
                '-'
            };
            format_permissions(type_char, m.permissions().mode())
        }
        None => "l?????????".to_owned(),
    };
    let type_text = type_description(&name, kind, is_symlink, metadata.is_none());

    Entry {
        file_name,
        name: name.into(),
        kind,
        size_text: size_text.into(),
        modified_text: modified_text.into(),
        permissions_text: permissions_text.into(),
        type_text: type_text.into(),
    }
}

fn extension(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
}

pub fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Dir;
    }
    match extension(name) {
        Some(ext) if IMAGE_EXTENSIONS.contains(&ext.as_str()) => Kind::Image,
        _ => Kind::File,
    }
}

fn type_description(name: &str, kind: Kind, is_symlink: bool, is_broken: bool) -> String {
    if is_broken {
        return "Broken link".to_owned();
    }
    let base = match (kind, extension(name)) {
        (Kind::Dir, _) => "Folder".to_owned(),
        (Kind::Image, Some(ext)) => format!("{} image", ext.to_uppercase()),
        (_, Some(ext)) => format!("{} file", ext.to_uppercase()),
        (_, None) => "File".to_owned(),
    };
    if is_symlink {
        format!("Link · {base}")
    } else {
        base
    }
}

/// Folders first; within each group, alphabetical order ignoring case.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| compare_names(&a.name, &b.name))
    });
}

pub fn compare_names(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
        .then_with(|| a.cmp(b))
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let number = if value < 10.0 {
        let text = format!("{value:.1}");
        text.strip_suffix(".0").map(str::to_owned).unwrap_or(text)
    } else {
        format!("{value:.0}")
    };
    format!("{number} {}", UNITS[unit])
}

/// `today 14:02`, `yesterday 18:40`, `Nov 12 09:12`; with the year instead of the time if it is
/// not the current one.
pub fn format_modified(modified: DateTime, today: Date) -> String {
    let date = modified.date();
    let time = format!("{:02}:{:02}", modified.hour(), modified.minute());
    let month = MONTHS[(date.month() - 1) as usize];
    if date == today {
        format!("today {time}")
    } else if today.yesterday().is_ok_and(|yesterday| yesterday == date) {
        format!("yesterday {time}")
    } else if date.year() == today.year() {
        format!("{month} {} {time}", date.day())
    } else {
        format!("{month} {} {}", date.day(), date.year())
    }
}

/// `ls -l` format, including setuid, setgid and sticky.
pub fn format_permissions(type_char: char, mode: u32) -> String {
    const BITS: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut text = String::with_capacity(10);
    text.push(type_char);
    for (index, (bit, letter)) in BITS.iter().enumerate() {
        let set = mode & bit != 0;
        let special = match index {
            2 => Some((0o4000, 's')),
            5 => Some((0o2000, 's')),
            8 => Some((0o1000, 't')),
            _ => None,
        };
        text.push(match special {
            Some((special_bit, symbol)) if mode & special_bit != 0 => {
                if set {
                    symbol
                } else {
                    symbol.to_ascii_uppercase()
                }
            }
            _ if set => *letter,
            _ => '-',
        });
    }
    text
}

/// Initial cursor row when a folder opens. `select` is the entry to mark, such as the folder that
/// was left when going up. If it is not there, the first real entry is chosen, skipping `..`.
pub fn initial_cursor(entries: &[Entry], select: Option<&OsStr>, has_parent: bool) -> usize {
    let offset = usize::from(has_parent);
    if let Some(index) = select.and_then(|name| entries.iter().position(|e| e.file_name == name)) {
        return index + offset;
    }
    if entries.is_empty() { 0 } else { offset }
}

/// Cursor row when the same folder is read again: the entry `select`, which was under the cursor,
/// or the same position `previous` if it is gone.
pub fn reload_cursor(
    entries: &[Entry],
    select: Option<&OsStr>,
    has_parent: bool,
    previous: usize,
) -> usize {
    let offset = usize::from(has_parent);
    if let Some(index) = select.and_then(|name| entries.iter().position(|e| e.file_name == name)) {
        return index + offset;
    }
    previous.min((entries.len() + offset).saturating_sub(1))
}

/// Path for display, with `~` instead of the home folder.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[allow(dead_code, reason = "the Places sidebar is hidden for now")]
pub struct Place {
    pub name: SharedString,
    pub path: PathBuf,
    pub icon: &'static str,
}

/// Home and the user's standard folders that exist. Honors `~/.config/user-dirs.dirs` when it
/// exists, as on systems with localized folder names.
#[allow(dead_code, reason = "the Places sidebar is hidden for now")]
pub fn places(home: &Path) -> Vec<Place> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let user_dirs = fs::read_to_string(config.join("user-dirs.dirs"))
        .map(|content| parse_user_dirs(&content, home))
        .unwrap_or_default();

    let standard = [
        (
            "Documents",
            "XDG_DOCUMENTS_DIR",
            "Documents",
            icon::DOCUMENT,
        ),
        ("Downloads", "XDG_DOWNLOAD_DIR", "Downloads", icon::DOWNLOAD),
        ("Pictures", "XDG_PICTURES_DIR", "Pictures", icon::IMAGE),
        ("Music", "XDG_MUSIC_DIR", "Music", icon::MUSIC),
        ("Videos", "XDG_VIDEOS_DIR", "Videos", icon::VIDEO),
    ];
    let mut places = vec![Place {
        name: "Home".into(),
        path: home.to_path_buf(),
        icon: icon::HOME,
    }];
    for (name, key, default, icon) in standard {
        let path = user_dirs
            .get(key)
            .cloned()
            .unwrap_or_else(|| home.join(default));
        // A disabled XDG folder points to the home folder: it is not listed twice.
        if path != home && path.is_dir() {
            places.push(Place {
                name: name.into(),
                path,
                icon,
            });
        }
    }
    places
}

/// Parses lines like `XDG_..._DIR="$HOME/Documents"` or with an absolute path.
#[allow(dead_code, reason = "the Places sidebar is hidden for now")]
pub fn parse_user_dirs(content: &str, home: &Path) -> HashMap<String, PathBuf> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"');
            let path = match value.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None if value.starts_with('/') => PathBuf::from(value),
                None => return None,
            };
            Some((key.trim().to_owned(), path))
        })
        .collect()
}

/// The most specific place that contains the current folder.
#[allow(dead_code, reason = "the Places sidebar is hidden for now")]
pub fn active_place(places: &[Place], cwd: &Path) -> Option<usize> {
    places
        .iter()
        .enumerate()
        .filter(|(_, place)| cwd.starts_with(&place.path))
        .max_by_key(|(_, place)| place.path.components().count())
        .map(|(index, _)| index)
}

pub fn describe_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        io::ErrorKind::NotFound => "the folder does not exist".to_owned(),
        _ => error.to_string(),
    }
}

/// Text shown in the path bar when the typed path cannot be opened.
pub fn describe_path_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "No such path".to_owned(),
        io::ErrorKind::NotADirectory => "Part of the path is not a folder".to_owned(),
        io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => error.to_string(),
    }
}

/// Path typed in the bar: a leading `~` is the home folder and a relative path starts at `cwd`.
/// `.` and `..` are resolved on the text, without touching the disk, as when navigating with `h`.
pub fn resolve_input(text: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let text = text.trim();
    let expanded = match (text.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(text),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// Folder to open in order to show `path`: the same one if it is a folder; if it is a file (or a
/// broken link), the folder that contains it, with it under the cursor.
pub fn locate(path: &Path) -> io::Result<(PathBuf, Option<OsString>)> {
    let metadata = fs::metadata(path).or_else(|error| match error.kind() {
        io::ErrorKind::NotFound => fs::symlink_metadata(path).map_err(|_| error),
        _ => Err(error),
    })?;
    if metadata.is_dir() {
        return Ok((path.to_path_buf(), None));
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent.to_path_buf(), Some(name.to_os_string()))),
        _ => Err(io::ErrorKind::NotADirectory.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            file_name: name.into(),
            name: name.to_owned().into(),
            kind: kind_of(name, is_dir),
            size_text: SharedString::default(),
            modified_text: SharedString::default(),
            permissions_text: SharedString::default(),
            type_text: SharedString::default(),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_ref()).collect()
    }

    #[test]
    fn sorts_dirs_first_then_case_insensitive() {
        let mut entries = vec![
            entry("zeta.txt", false),
            entry("Beta", true),
            entry("alpha.png", false),
            entry(".config", true),
            entry("alpha", true),
            entry("Alpha.png", false),
        ];
        sort_entries(&mut entries);
        assert_eq!(
            names(&entries),
            [
                ".config",
                "alpha",
                "Beta",
                "Alpha.png",
                "alpha.png",
                "zeta.txt"
            ]
        );
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(412 * 1024), "412 KB");
        assert_eq!(format_size(5 * 1024 * 1024 + 300 * 1024), "5.3 MB");
        assert_eq!(format_size(1_200 * 1024 * 1024 * 1024), "1.2 TB");
    }

    #[test]
    fn formats_modification_dates() {
        let today = date(2026, 11, 15);
        assert_eq!(format_modified(today.at(14, 2, 0, 0), today), "today 14:02");
        assert_eq!(
            format_modified(date(2026, 11, 14).at(18, 40, 0, 0), today),
            "yesterday 18:40"
        );
        assert_eq!(
            format_modified(date(2026, 11, 12).at(9, 12, 0, 0), today),
            "Nov 12 09:12"
        );
        assert_eq!(
            format_modified(date(2025, 1, 3).at(9, 12, 0, 0), today),
            "Jan 3 2025"
        );
        // Yesterday across the turn of the year.
        let new_year = date(2027, 1, 1);
        assert_eq!(
            format_modified(date(2026, 12, 31).at(23, 59, 0, 0), new_year),
            "yesterday 23:59"
        );
    }

    #[test]
    fn formats_permissions() {
        assert_eq!(format_permissions('d', 0o755), "drwxr-xr-x");
        assert_eq!(format_permissions('-', 0o644), "-rw-r--r--");
        assert_eq!(format_permissions('-', 0o4755), "-rwsr-xr-x");
        assert_eq!(format_permissions('d', 0o1777), "drwxrwxrwt");
        assert_eq!(format_permissions('-', 0o2640), "-rw-r-S---");
    }

    #[test]
    fn classifies_images_by_extension() {
        assert_eq!(kind_of("banner.PNG", false), Kind::Image);
        assert_eq!(kind_of("logo.svg", false), Kind::File);
        assert_eq!(kind_of(".bashrc", false), Kind::File);
        assert_eq!(kind_of("photos.jpg", true), Kind::Dir);
    }

    #[test]
    fn cursor_returns_to_the_folder_left_behind() {
        let entries = vec![entry("a", true), entry("b", true), entry("c.txt", false)];
        assert_eq!(initial_cursor(&entries, Some(OsStr::new("b")), true), 2);
        assert_eq!(initial_cursor(&entries, Some(OsStr::new("b")), false), 1);
        assert_eq!(initial_cursor(&entries, Some(OsStr::new("x")), true), 1);
        assert_eq!(initial_cursor(&entries, None, true), 1);
        assert_eq!(initial_cursor(&[], None, true), 0);
    }

    #[test]
    fn reload_keeps_the_cursor() {
        let entries = vec![entry("a", true), entry("new", false), entry("z.txt", false)];
        // The entry is still there (though it moved).
        assert_eq!(
            reload_cursor(&entries, Some(OsStr::new("z.txt")), true, 2),
            3
        );
        assert_eq!(reload_cursor(&entries, Some(OsStr::new("a")), false, 4), 0);
        // It is gone: same position, without going past the last row.
        assert_eq!(
            reload_cursor(&entries, Some(OsStr::new("deleted")), true, 2),
            2
        );
        assert_eq!(
            reload_cursor(&entries, Some(OsStr::new("deleted")), true, 9),
            3
        );
        // The cursor was on `..`.
        assert_eq!(reload_cursor(&entries, None, true, 0), 0);
        assert_eq!(reload_cursor(&[], None, true, 3), 0);
        assert_eq!(reload_cursor(&[], None, false, 3), 0);
    }

    #[test]
    fn displays_home_as_tilde() {
        let home = Path::new("/home/user");
        assert_eq!(display_path(home, Some(home)), "~");
        assert_eq!(
            display_path(Path::new("/home/user/projects/x"), Some(home)),
            "~/projects/x"
        );
        assert_eq!(display_path(Path::new("/usr/lib"), Some(home)), "/usr/lib");
        assert_eq!(
            display_path(Path::new("/home/username"), Some(home)),
            "/home/username"
        );
    }

    #[test]
    fn parses_user_dirs() {
        let home = Path::new("/home/user");
        let dirs = parse_user_dirs(
            // A localized folder name, as user-dirs.dirs has on non-English systems.
            "# comment\nXDG_DOCUMENTS_DIR=\"$HOME/Documentos\"\nXDG_MUSIC_DIR=\"/media/music\"\nXDG_DESKTOP_DIR=\"$HOME/\"\n",
            home,
        );
        assert_eq!(
            dirs["XDG_DOCUMENTS_DIR"],
            Path::new("/home/user/Documentos")
        );
        assert_eq!(dirs["XDG_MUSIC_DIR"], Path::new("/media/music"));
        assert_eq!(dirs["XDG_DESKTOP_DIR"], home);
    }

    #[test]
    fn active_place_is_the_most_specific() {
        let place = |path: &str| Place {
            name: SharedString::default(),
            path: PathBuf::from(path),
            icon: icon::FOLDER,
        };
        let places = vec![place("/home/u"), place("/home/u/Documents")];
        assert_eq!(
            active_place(&places, Path::new("/home/u/Documents/x")),
            Some(1)
        );
        assert_eq!(active_place(&places, Path::new("/home/u/Music")), Some(0));
        assert_eq!(active_place(&places, Path::new("/etc")), None);
    }

    #[test]
    fn reads_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("exporer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Sub")).unwrap();
        fs::write(dir.join("b.txt"), "text").unwrap();
        fs::write(dir.join("A.png"), []).unwrap();
        std::os::unix::fs::symlink(dir.join("Sub"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("nothing"), dir.join("broken")).unwrap();

        let entries = read_dir(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(names(&entries), ["link", "Sub", "A.png", "b.txt", "broken"]);
        assert_eq!(entries[0].kind, Kind::Dir);
        assert!(entries[0].permissions_text.starts_with('l'));
        assert_eq!(entries[0].type_text.as_ref(), "Link · Folder");
        assert_eq!(entries[2].kind, Kind::Image);
        assert_eq!(entries[3].size_text.as_ref(), "4 B");
        assert!(entries[3].modified_text.starts_with("today "));
        assert_eq!(entries[4].type_text.as_ref(), "Broken link");
    }

    #[test]
    fn resolves_typed_paths() {
        let cwd = Path::new("/home/u/projects/app");
        let home = Some(Path::new("/home/u"));
        let resolve = |text: &str| resolve_input(text, cwd, home);
        assert_eq!(resolve("~"), PathBuf::from("/home/u"));
        assert_eq!(resolve("~/"), PathBuf::from("/home/u"));
        assert_eq!(resolve("~/Documents/"), PathBuf::from("/home/u/Documents"));
        assert_eq!(
            resolve("  /usr/./lib/../share  "),
            PathBuf::from("/usr/share")
        );
        assert_eq!(resolve("src"), PathBuf::from("/home/u/projects/app/src"));
        assert_eq!(resolve("../lib"), PathBuf::from("/home/u/projects/lib"));
        assert_eq!(resolve("/.."), PathBuf::from("/"));
        assert_eq!(resolve("/"), PathBuf::from("/"));
        assert_eq!(resolve(""), cwd);
        // `~user` is not expanded: it is a relative name.
        assert_eq!(resolve("~x"), PathBuf::from("/home/u/projects/app/~x"));
        assert_eq!(
            resolve_input("~/a", cwd, None),
            PathBuf::from("/home/u/projects/app/~/a")
        );
    }

    #[test]
    fn locates_folders_and_files() {
        let dir = std::env::temp_dir().join(format!("exporer-locate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Sub")).unwrap();
        fs::write(dir.join("a.txt"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("nothing"), dir.join("broken")).unwrap();

        let folder = locate(&dir.join("Sub")).unwrap();
        let file = locate(&dir.join("a.txt")).unwrap();
        let broken = locate(&dir.join("broken")).unwrap();
        let missing = locate(&dir.join("missing")).unwrap_err();
        let not_dir = locate(&dir.join("a.txt/x")).unwrap_err();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(folder, (dir.join("Sub"), None));
        assert_eq!(file, (dir.clone(), Some("a.txt".into())));
        assert_eq!(broken, (dir.clone(), Some("broken".into())));
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert_eq!(describe_path_error(&missing), "No such path");
        assert_eq!(not_dir.kind(), io::ErrorKind::NotADirectory);
    }
}

//! Operations on files, with no dependency on the UI: copying and moving entries into a folder,
//! and the register that `yy`/`xx` fill and `pp` pastes.
//!
//! Nothing is ever overwritten: an entry whose name is taken is pasted as `name (1).ext`.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
}

impl Op {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Copy => b"copy",
            Self::Move => b"move",
        }
    }
}

/// Shared between the thread doing the work and the UI that shows it and can cancel it.
#[derive(Default)]
pub struct Progress {
    /// Top-level entries finished, well or not.
    done: AtomicUsize,
    cancel: AtomicBool,
}

impl Progress {
    pub fn done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    /// The work stops before the next file; what was already done stays.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct Failure {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Where each entry ended up (pasted, or in the trash), in the order of the sources; for a
    /// permanent delete, the deleted paths.
    pub done: Vec<PathBuf>,
    pub failures: Vec<Failure>,
    /// Sources that were not processed because the work was cancelled.
    pub skipped: Vec<PathBuf>,
}

enum Error {
    Io(io::Error),
    Cancelled,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Copies or moves `sources` into `dir`. Each source that fails is reported and the rest go on.
pub fn paste(op: Op, sources: &[PathBuf], dir: &Path, progress: &Progress) -> Report {
    let mut report = Report::default();
    for (index, source) in sources.iter().enumerate() {
        if progress.cancelled() {
            report.skipped.extend_from_slice(&sources[index..]);
            break;
        }
        match paste_one(op, source, dir, progress) {
            Ok(target) => report.done.push(target),
            Err(Error::Cancelled) => {
                report.skipped.extend_from_slice(&sources[index..]);
                break;
            }
            Err(Error::Io(error)) => report.failures.push(Failure {
                path: source.clone(),
                reason: describe(&error),
            }),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    report
}

fn paste_one(op: Op, source: &Path, dir: &Path, progress: &Progress) -> Result<PathBuf, Error> {
    let meta = fs::symlink_metadata(source)?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "it has no name"))?;
    if meta.is_dir() && is_inside(dir, source) {
        return Err(io::Error::other("cannot paste a folder into itself").into());
    }
    if op == Op::Move && source.parent() == Some(dir) {
        // Cut and pasted in the same folder: nothing to do.
        return Ok(source.to_path_buf());
    }
    loop {
        let target = dir.join(free_name(dir, name, meta.is_dir()));
        let result = match op {
            Op::Copy => copy_new(source, &target, progress),
            Op::Move => move_new(source, &target, progress),
        };
        match result {
            // Someone took the name between choosing it and using it: choose again.
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
            Ok(()) => return Ok(target),
        }
    }
}

/// `dir` is `folder` or inside it, comparing real paths when they can be resolved.
fn is_inside(dir: &Path, folder: &Path) -> bool {
    let resolve = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(dir).starts_with(resolve(folder))
}

/// `name` if nothing in `dir` has it; otherwise the first free `name (n)`. For files the number
/// goes before the last extension (`a (1).txt`); for folders and hidden files without another
/// dot, at the end (`dir (1)`, `.bashrc (1)`).
pub fn free_name(dir: &Path, name: &OsStr, is_dir: bool) -> OsString {
    let taken = |candidate: &OsStr| fs::symlink_metadata(dir.join(candidate)).is_ok();
    if !taken(name) {
        return name.to_owned();
    }
    (1..)
        .map(|n| numbered(name, n, is_dir))
        .find(|candidate| !taken(candidate))
        .expect("some number is free")
}

/// `name` with ` (n)` before the last extension of a file, or at the end of a folder or a hidden
/// name without another dot. `n == 0` is the name as it is.
fn numbered(name: &OsStr, n: usize, is_dir: bool) -> OsString {
    if n == 0 {
        return name.to_owned();
    }
    let bytes = name.as_bytes();
    let split = match bytes.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 && !is_dir => dot,
        _ => bytes.len(),
    };
    let (stem, extension) = bytes.split_at(split);
    let mut candidate = stem.to_vec();
    candidate.extend_from_slice(format!(" ({n})").as_bytes());
    candidate.extend_from_slice(extension);
    OsString::from_vec(candidate)
}

/// Copies `source` to `target`, which must not exist. Folders are copied with their content,
/// symbolic links as links (not followed) and permissions are kept. If it fails or is cancelled,
/// what was copied stays.
fn copy_new(source: &Path, target: &Path, progress: &Progress) -> Result<(), Error> {
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    let meta = fs::symlink_metadata(source)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    } else if kind.is_dir() {
        fs::create_dir(target)?;
        // Writable while it is filled, even if the original is not.
        fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
        let mut children: Vec<_> = fs::read_dir(source)?.collect::<io::Result<_>>()?;
        children.sort_by_key(fs::DirEntry::file_name);
        let result = children.iter().try_for_each(|child| {
            copy_new(&child.path(), &target.join(child.file_name()), progress)
        });
        fs::set_permissions(target, meta.permissions())?;
        result?;
    } else if kind.is_file() {
        let mut from = File::open(source)?;
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.permissions().mode())
            .open(target)?;
        io::copy(&mut from, &mut to)?;
        fs::set_permissions(target, meta.permissions())?;
    } else {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "special files are not copied").into());
    }
    Ok(())
}

/// Moves `source` to `target`, which must not exist. Across file systems it copies and then
/// removes the source, only if the copy was complete; otherwise the partial copy is removed and
/// the source stays as it was.
fn move_new(source: &Path, target: &Path, progress: &Progress) -> Result<(), Error> {
    match rename_no_replace(source, target) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            if let Err(error) = copy_new(source, target, progress) {
                // Only remove what this copy created.
                if !matches!(&error, Error::Io(e) if e.kind() == io::ErrorKind::AlreadyExists) {
                    remove(target).ok();
                }
                return Err(error);
            }
            remove(source)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn rename_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths; AT_FDCWD resolves them like `rename`.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // The file system does not support the flag: check by hand. A name taken between the
        // check and the rename would be replaced, which `renameat2` exists to avoid.
        Some(libc::EINVAL) => {
            if fs::symlink_metadata(target).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(source, target)
        }
        _ => Err(error),
    }
}

/// Removes a file, a link (not what it points to) or a folder with its content.
pub fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// An error for the user, without Rust's `(os error N)` suffix.
pub fn describe(error: &io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_owned(),
        None => text,
    }
}

/// Removes `paths` for good, one after the other; folders with their content and links without
/// following them.
pub fn delete_all(paths: &[PathBuf], progress: &Progress) -> Report {
    each(paths, progress, |path| remove(path).map(|()| path.to_path_buf()))
}

/// Moves `paths` to the trash. Without a trash (no home folder), every entry fails: nothing is
/// ever deleted instead.
pub fn trash_all(trash: Option<&Trash>, paths: &[PathBuf], progress: &Progress) -> Report {
    each(paths, progress, |path| match trash {
        Some(trash) => trash.put(path),
        None => Err(io::Error::other("there is no trash folder")),
    })
}

/// Runs `work` on each path, stopping before the next one if cancelled.
fn each(
    paths: &[PathBuf],
    progress: &Progress,
    mut work: impl FnMut(&Path) -> io::Result<PathBuf>,
) -> Report {
    let mut report = Report::default();
    for (index, path) in paths.iter().enumerate() {
        if progress.cancelled() {
            report.skipped.extend_from_slice(&paths[index..]);
            break;
        }
        match work(path) {
            Ok(done) => report.done.push(done),
            Err(error) => report.failures.push(Failure {
                path: path.clone(),
                reason: describe(&error),
            }),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    report
}

/// The trash of the freedesktop.org specification (Trash 1.0), the one Dolphin and Nautilus use.
/// Entries on the file system of the home trash go there; entries on another one go to the trash
/// at the top of that file system.
pub struct Trash {
    /// `$XDG_DATA_HOME/Trash`, by default `~/.local/share/Trash`.
    home: PathBuf,
}

impl Trash {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn session() -> Option<Self> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|dir| Path::new(dir).is_absolute())
            .map(PathBuf::from)
            .or_else(|| {
                let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
                Some(PathBuf::from(home).join(".local/share"))
            })?;
        Some(Self::new(data.join("Trash")))
    }

    /// Moves `path` to the trash and returns where it ended up.
    pub fn put(&self, path: &Path) -> io::Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;

        let meta = fs::symlink_metadata(path)?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "it has no name"))?;
        let (trash, recorded) = if meta.dev() == device_of(&self.home)? {
            (self.home.clone(), path.to_path_buf())
        } else {
            let top = mount_root(path, meta.dev())?;
            let recorded = path.strip_prefix(&top).unwrap_or(path).to_path_buf();
            (topdir_trash(&top)?, recorded)
        };
        if path.starts_with(&trash) {
            return Err(io::Error::other("it is inside the trash"));
        }
        let files = trash.join("files");
        let info = trash.join("info");
        for dir in [&trash, &files, &info] {
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    // `create_dir_all` for the parents of the home trash (`~/.local/share`).
                    if error.kind() == io::ErrorKind::NotFound {
                        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
                    } else {
                        return Err(error);
                    }
                }
                _ => {}
            }
        }
        let date = jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S").to_string();
        let content = format!(
            "[Trash Info]\nPath={}\nDeletionDate={date}\n",
            encode_path(recorded.as_os_str().as_bytes())
        );
        for n in 0.. {
            let candidate = numbered(name, n, meta.is_dir());
            let mut info_name = candidate.clone();
            info_name.push(".trashinfo");
            let info_path = info.join(&info_name);
            // The info file is created first and exclusively: it reserves the name.
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&info_path)
            {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                result => result?,
            };
            if let Err(error) = file.write_all(content.as_bytes()) {
                fs::remove_file(&info_path).ok();
                return Err(error);
            }
            let target = files.join(&candidate);
            match rename_no_replace(path, &target) {
                Ok(()) => return Ok(target),
                Err(error) => {
                    fs::remove_file(&info_path).ok();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
            }
        }
        unreachable!("the loop only ends by returning")
    }
}

/// Device of `path`, or of its closest existing parent (the home trash may not exist yet).
fn device_of(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut current = Some(path);
    while let Some(path) = current {
        if let Ok(meta) = fs::metadata(path) {
            return Ok(meta.dev());
        }
        current = path.parent();
    }
    Err(io::ErrorKind::NotFound.into())
}

/// The top folder of the file system `device` that holds `path`.
fn mount_root(path: &Path, device: u64) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut top = path.parent().unwrap_or(path).to_path_buf();
    while let Some(parent) = top.parent() {
        if fs::metadata(parent)?.dev() != device {
            break;
        }
        top = parent.to_path_buf();
    }
    Ok(top)
}

/// The trash at the top of a file system: `$topdir/.Trash/$uid` if `.Trash` is a real folder
/// with the sticky bit, otherwise `$topdir/.Trash-$uid`, which must be a real folder of ours.
fn topdir_trash(top: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid cannot fail and has no preconditions.
    let uid = unsafe { libc::getuid() };
    if let Ok(meta) = fs::symlink_metadata(top.join(".Trash"))
        && meta.is_dir()
        && meta.mode() & 0o1000 != 0
    {
        return Ok(top.join(".Trash").join(uid.to_string()));
    }
    let own = top.join(format!(".Trash-{uid}"));
    match fs::symlink_metadata(&own) {
        Ok(meta) if !meta.is_dir() || meta.uid() != uid => Err(io::Error::other(format!(
            "{} is not a valid trash folder",
            own.display()
        ))),
        _ => Ok(own),
    }
}

/// A path for a `.trashinfo` file: every byte but unreserved ones and `/` as `%XX`.
fn encode_path(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            text.push(byte as char);
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

/// What `yy` or `xx` saved for `pp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    pub op: Op,
    pub paths: Vec<PathBuf>,
}

impl Register {
    /// `copy` or `move`, then each path, all ended by NUL: paths may hold any byte but NUL.
    fn encode(&self) -> Vec<u8> {
        let mut bytes = self.op.tag().to_vec();
        bytes.push(0);
        for path in &self.paths {
            bytes.extend_from_slice(path.as_os_str().as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut fields = bytes.strip_suffix(&[0])?.split(|&b| b == 0);
        let op = match fields.next()? {
            b"copy" => Op::Copy,
            b"move" => Op::Move,
            _ => return None,
        };
        let paths: Vec<PathBuf> = fields
            .map(|field| PathBuf::from(OsStr::from_bytes(field)))
            .collect();
        (!paths.is_empty()).then_some(Self { op, paths })
    }
}

/// Where the register lives. With a file, every exporer window of the session shares it;
/// without one (no `XDG_RUNTIME_DIR`), it stays in this process.
pub struct RegisterStore {
    file: Option<PathBuf>,
    memory: Option<Arc<Register>>,
    /// The file as last read: it is only read again when its modification time or size change,
    /// since the view asks on every frame.
    cache: Option<(SystemTime, u64, Option<Arc<Register>>)>,
}

impl RegisterStore {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            memory: None,
            cache: None,
        }
    }

    /// `$XDG_RUNTIME_DIR/exporer/register`.
    pub fn session() -> Self {
        let file = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .map(|dir| PathBuf::from(dir).join("exporer").join("register"));
        Self::new(file)
    }

    pub fn load(&mut self) -> Option<Arc<Register>> {
        let Some(file) = &self.file else {
            return self.memory.clone();
        };
        let meta = fs::metadata(file).ok()?;
        let stamp = (meta.modified().ok()?, meta.len());
        if let Some((modified, len, register)) = &self.cache
            && (*modified, *len) == stamp
        {
            return register.clone();
        }
        let register = Register::decode(&fs::read(file).ok()?).map(Arc::new);
        self.cache = Some((stamp.0, stamp.1, register.clone()));
        register
    }

    /// Replaces the register; `None` empties it.
    pub fn save(&mut self, register: Option<&Register>) -> io::Result<()> {
        let Some(file) = &self.file else {
            self.memory = register.cloned().map(Arc::new);
            return Ok(());
        };
        self.cache = None;
        let Some(register) = register else {
            return match fs::remove_file(file) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        };
        let dir = file.parent().expect("the file is inside a folder");
        fs::create_dir_all(dir)?;
        // Written aside and renamed, so another window never reads half of it.
        let temporary = dir.join(format!("register.{}", std::process::id()));
        fs::write(&temporary, register.encode())?;
        fs::rename(&temporary, file)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// A fresh folder under the system's temporary folder, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "exporer-fileops-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Folders copied without write permission could not be emptied otherwise.
            let _ = std::process::Command::new("chmod")
                .args(["-R", "u+rwx"])
                .arg(&self.0)
                .status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn free_names_never_reuse_a_taken_one() {
        let s = Scratch::new();
        for name in ["a.txt", "a (1).txt", ".bashrc", "dir", "x.tar.gz", "noext"] {
            fs::write(s.path(name), "").unwrap();
        }
        let free = |name: &str, is_dir| free_name(&s.0, OsStr::new(name), is_dir);
        assert_eq!(free("new.txt", false), "new.txt");
        assert_eq!(free("a.txt", false), "a (2).txt");
        assert_eq!(free(".bashrc", false), ".bashrc (1)");
        assert_eq!(free("dir", true), "dir (1)");
        assert_eq!(free("x.tar.gz", false), "x.tar (1).gz");
        assert_eq!(free("noext", false), "noext (1)");
    }

    #[test]
    fn copies_folders_links_and_permissions() {
        let s = Scratch::new();
        fs::create_dir_all(s.path("src/tree/inner")).unwrap();
        fs::write(s.path("src/tree/inner/file.txt"), "hello").unwrap();
        fs::write(s.path("src/tree/script.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(s.path("src/tree/script.sh"), fs::Permissions::from_mode(0o751))
            .unwrap();
        symlink("inner/file.txt", s.path("src/tree/link")).unwrap();
        symlink("/nowhere", s.path("src/tree/broken")).unwrap();
        fs::set_permissions(s.path("src/tree/inner"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::create_dir(s.path("dst")).unwrap();

        let report = paste(Op::Copy, &[s.path("src/tree")], &s.path("dst"), &Progress::default());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.done, [s.path("dst/tree")]);
        assert_eq!(
            fs::read_to_string(s.path("dst/tree/inner/file.txt")).unwrap(),
            "hello"
        );
        let mode = |p: &str| fs::metadata(s.path(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("dst/tree/script.sh"), 0o751);
        assert_eq!(mode("dst/tree/inner"), 0o555);
        assert_eq!(
            fs::read_link(s.path("dst/tree/link")).unwrap(),
            Path::new("inner/file.txt")
        );
        assert_eq!(
            fs::read_link(s.path("dst/tree/broken")).unwrap(),
            Path::new("/nowhere")
        );
        // The source is untouched.
        assert!(s.path("src/tree/inner/file.txt").exists());
    }

    #[test]
    fn pasting_twice_in_the_same_folder_duplicates() {
        let s = Scratch::new();
        fs::write(s.path("a.txt"), "1").unwrap();
        fs::create_dir(s.path("dir")).unwrap();
        let sources = [s.path("a.txt"), s.path("dir")];
        let first = paste(Op::Copy, &sources, &s.0, &Progress::default());
        let second = paste(Op::Copy, &sources, &s.0, &Progress::default());
        assert_eq!(first.done, [s.path("a (1).txt"), s.path("dir (1)")]);
        assert_eq!(second.done, [s.path("a (2).txt"), s.path("dir (2)")]);
        assert_eq!(fs::read_to_string(s.path("a (2).txt")).unwrap(), "1");
    }

    #[test]
    fn moves_without_replacing() {
        let s = Scratch::new();
        fs::create_dir_all(s.path("from")).unwrap();
        fs::create_dir_all(s.path("to")).unwrap();
        fs::write(s.path("from/a.txt"), "new").unwrap();
        fs::write(s.path("to/a.txt"), "old").unwrap();
        let report = paste(Op::Move, &[s.path("from/a.txt")], &s.path("to"), &Progress::default());
        assert_eq!(report.done, [s.path("to/a (1).txt")]);
        assert_eq!(fs::read_to_string(s.path("to/a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(s.path("to/a (1).txt")).unwrap(), "new");
        assert!(names(&s.path("from")).is_empty());
    }

    /// `/tmp` is usually a tmpfs and `target/` is on the disk: `rename` gives `EXDEV` and the move
    /// copies and removes. Skipped if both are on the same file system.
    #[test]
    fn moves_across_file_systems() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new();
        let disk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("fileops-test-{}", std::process::id()));
        fs::create_dir_all(&disk).unwrap();
        let same = fs::metadata(&s.0).unwrap().dev() == fs::metadata(&disk).unwrap().dev();
        if !same {
            fs::create_dir_all(s.path("tree/inner")).unwrap();
            fs::write(s.path("tree/inner/f"), "data").unwrap();
            symlink("inner/f", s.path("tree/link")).unwrap();
            let report = paste(Op::Move, &[s.path("tree")], &disk, &Progress::default());
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert_eq!(fs::read_to_string(disk.join("tree/inner/f")).unwrap(), "data");
            assert!(disk.join("tree/link").symlink_metadata().unwrap().is_symlink());
            assert!(!s.path("tree").exists());
        }
        fs::remove_dir_all(&disk).unwrap();
    }

    #[test]
    fn moving_into_the_same_folder_does_nothing() {
        let s = Scratch::new();
        fs::write(s.path("a.txt"), "").unwrap();
        let report = paste(Op::Move, &[s.path("a.txt")], &s.0, &Progress::default());
        assert_eq!(report.done, [s.path("a.txt")]);
        assert_eq!(names(&s.0), ["a.txt"]);
    }

    #[test]
    fn a_folder_cannot_go_inside_itself_and_the_rest_goes_on() {
        let s = Scratch::new();
        fs::create_dir_all(s.path("dir/sub")).unwrap();
        fs::write(s.path("b.txt"), "").unwrap();
        let sources = [s.path("dir"), s.path("b.txt")];
        for op in [Op::Copy, Op::Move] {
            let report = paste(op, &sources, &s.path("dir/sub"), &Progress::default());
            assert_eq!(report.failures.len(), 1);
            assert_eq!(report.failures[0].path, s.path("dir"));
            assert_eq!(report.failures[0].reason, "cannot paste a folder into itself");
            assert_eq!(report.done.len(), 1);
            if op == Op::Move {
                assert!(s.path("dir/sub/b.txt").exists());
            }
        }
    }

    #[test]
    fn missing_sources_fail_alone() {
        let s = Scratch::new();
        fs::write(s.path("a.txt"), "").unwrap();
        fs::create_dir(s.path("to")).unwrap();
        let report = paste(
            Op::Copy,
            &[s.path("gone"), s.path("a.txt")],
            &s.path("to"),
            &Progress::default(),
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].reason, "No such file or directory");
        assert_eq!(names(&s.path("to")), ["a.txt"]);
    }

    #[test]
    fn cancelling_stops_before_the_next_entry() {
        let s = Scratch::new();
        fs::write(s.path("a"), "").unwrap();
        fs::write(s.path("b"), "").unwrap();
        fs::create_dir(s.path("to")).unwrap();
        let progress = Progress::default();
        progress.cancel();
        let report = paste(Op::Copy, &[s.path("a"), s.path("b")], &s.path("to"), &progress);
        assert!(report.done.is_empty());
        assert_eq!(report.skipped, [s.path("a"), s.path("b")]);
        assert!(names(&s.path("to")).is_empty());
    }

    #[test]
    fn counts_progress() {
        let s = Scratch::new();
        fs::write(s.path("a"), "").unwrap();
        fs::create_dir(s.path("to")).unwrap();
        let progress = Progress::default();
        paste(Op::Copy, &[s.path("a"), s.path("gone")], &s.path("to"), &progress);
        assert_eq!(progress.done(), 2);
    }

    fn trash_info(trash: &Path, name: &str) -> String {
        fs::read_to_string(trash.join("info").join(format!("{name}.trashinfo"))).unwrap()
    }

    #[test]
    fn trashes_with_an_info_file_and_distinct_names() {
        let s = Scratch::new();
        let trash = Trash::new(s.path("data/Trash"));
        fs::create_dir_all(s.path("one/dir")).unwrap();
        fs::create_dir_all(s.path("two")).unwrap();
        fs::write(s.path("one/a b%.txt"), "1").unwrap();
        fs::write(s.path("two/a b%.txt"), "2").unwrap();
        fs::write(s.path("one/dir/inner"), "").unwrap();
        let sources = [s.path("one/a b%.txt"), s.path("two/a b%.txt"), s.path("one/dir")];
        let report = trash_all(Some(&trash), &sources, &Progress::default());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let files = s.path("data/Trash/files");
        assert_eq!(
            report.done,
            [files.join("a b%.txt"), files.join("a b% (1).txt"), files.join("dir")]
        );
        assert_eq!(fs::read_to_string(files.join("a b% (1).txt")).unwrap(), "2");
        assert!(files.join("dir/inner").exists());
        assert!(!s.path("one/dir").exists());

        let info = trash_info(&s.path("data/Trash"), "a b% (1).txt");
        let mut lines = info.lines();
        assert_eq!(lines.next(), Some("[Trash Info]"));
        let encoded = encode_path(s.path("two").as_os_str().as_bytes());
        assert_eq!(lines.next(), Some(format!("Path={encoded}/a%20b%25.txt").as_str()));
        let date = lines.next().unwrap().strip_prefix("DeletionDate=").unwrap();
        assert!(jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S", date).is_ok(), "{date}");
        let mode = fs::metadata(s.path("data/Trash")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn trashing_without_a_trash_never_deletes() {
        let s = Scratch::new();
        fs::write(s.path("a"), "").unwrap();
        let report = trash_all(None, &[s.path("a")], &Progress::default());
        assert_eq!(report.failures.len(), 1);
        assert!(s.path("a").exists());
    }

    #[test]
    fn missing_entries_fail_to_trash() {
        let s = Scratch::new();
        let trash = Trash::new(s.path("Trash"));
        let report = trash_all(Some(&trash), &[s.path("gone")], &Progress::default());
        assert_eq!(report.failures[0].reason, "No such file or directory");
    }

    #[test]
    fn chooses_the_trash_at_the_top_of_a_file_system() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new();
        let uid = fs::metadata(&s.0).unwrap().uid();
        assert_eq!(topdir_trash(&s.0).unwrap(), s.path(&format!(".Trash-{uid}")));
        // A `.Trash` without the sticky bit is not used.
        fs::create_dir(s.path(".Trash")).unwrap();
        assert_eq!(topdir_trash(&s.0).unwrap(), s.path(&format!(".Trash-{uid}")));
        fs::set_permissions(s.path(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(topdir_trash(&s.0).unwrap(), s.path(&format!(".Trash/{uid}")));
        // A `.Trash-$uid` that is a file is refused.
        let other = Scratch::new();
        fs::write(other.path(&format!(".Trash-{uid}")), "").unwrap();
        assert!(topdir_trash(&other.0).is_err());
    }

    #[test]
    fn finds_the_top_of_a_file_system() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new();
        fs::create_dir_all(s.path("x/y")).unwrap();
        let device = fs::metadata(s.path("x/y")).unwrap().dev();
        let top = mount_root(&s.path("x/y"), device).unwrap();
        assert!(s.0.starts_with(&top));
        assert_eq!(fs::metadata(&top).unwrap().dev(), device);
        if let Some(parent) = top.parent() {
            assert_ne!(fs::metadata(parent).unwrap().dev(), device);
        }
    }

    #[test]
    fn deletes_folders_and_links_without_following_them() {
        let s = Scratch::new();
        fs::create_dir_all(s.path("dir/sub")).unwrap();
        fs::write(s.path("kept"), "").unwrap();
        symlink(s.path("kept"), s.path("link")).unwrap();
        symlink(s.path("dir"), s.path("dirlink")).unwrap();
        let sources = [s.path("dir"), s.path("link"), s.path("dirlink"), s.path("gone")];
        let report = delete_all(&sources, &Progress::default());
        assert_eq!(report.done.len(), 3);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(names(&s.0), ["kept"]);
    }

    #[test]
    fn encodes_trash_paths() {
        assert_eq!(encode_path(b"/home/u/a b.txt"), "/home/u/a%20b.txt");
        assert_eq!(encode_path("/ñ".as_bytes()), "/%C3%B1");
        assert_eq!(encode_path(b"/x\xff%"), "/x%FF%25");
    }

    #[test]
    fn the_register_round_trips_through_its_file() {
        let s = Scratch::new();
        let mut store = RegisterStore::new(Some(s.path("run/exporer/register")));
        assert_eq!(store.load(), None);
        let odd = PathBuf::from(OsString::from_vec(b"/tmp/odd\xff\nname".to_vec()));
        let register = Register {
            op: Op::Move,
            paths: vec![PathBuf::from("/a b/c"), odd],
        };
        store.save(Some(&register)).unwrap();
        // Another window reads the same file.
        let mut other = RegisterStore::new(Some(s.path("run/exporer/register")));
        assert_eq!(other.load().as_deref(), Some(&register));
        let replaced = Register {
            op: Op::Copy,
            paths: vec![PathBuf::from("/other/longer/path")],
        };
        store.save(Some(&replaced)).unwrap();
        assert_eq!(other.load().as_deref(), Some(&replaced));
        store.save(None).unwrap();
        assert_eq!(other.load(), None);
        store.save(None).unwrap();
    }

    #[test]
    fn the_register_stays_in_memory_without_a_file() {
        let mut store = RegisterStore::new(None);
        let register = Register {
            op: Op::Copy,
            paths: vec![PathBuf::from("/x")],
        };
        store.save(Some(&register)).unwrap();
        assert_eq!(store.load().as_deref(), Some(&register));
    }

    #[test]
    fn rejects_damaged_registers() {
        assert_eq!(Register::decode(b""), None);
        assert_eq!(Register::decode(b"copy\0"), None);
        assert_eq!(Register::decode(b"swap\0/a\0"), None);
        assert_eq!(Register::decode(b"copy\0/a"), None);
    }
}

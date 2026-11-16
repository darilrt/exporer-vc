//! Shell commands typed with `!` or `&`, with no dependency on the UI: `%s` substitution,
//! running with `sh -c`, detached launches, cleaning output for display, and history.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::mem;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use futures::channel::oneshot;
use gpui::SharedString;

/// Maximum output kept per command: past it, the oldest lines are dropped.
pub const OUTPUT_LIMIT: usize = 1 << 20;
/// How long to wait for pending output after `sh` exits. A process the command left in the
/// background (`program &`) can keep it open for as long as it lives.
const OUTPUT_GRACE: Duration = Duration::from_millis(100);
/// How long to wait for a program launched with `&` to report whether it failed right away.
pub const LAUNCH_GRACE: Duration = Duration::from_millis(500);
/// The thread that waits for a launched program only calls `wait`: a small stack is enough.
const LAUNCH_STACK: usize = 64 * 1024;
const HISTORY_LIMIT: usize = 100;
const TAB_WIDTH: usize = 8;

/// The command uses `%s` and there is no entry under the cursor.
#[derive(Debug, PartialEq, Eq)]
pub struct NothingSelected;

/// Replaces `%s` with `current` quoted and `%%` with `%`; any other `%` is left as is. Returns an
/// `OsString` because the file name may not be UTF-8.
pub fn expand(command: &str, current: Option<&OsStr>) -> Result<OsString, NothingSelected> {
    // `%` and `s` are ASCII: walking bytes never splits a character.
    let bytes = command.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            match bytes.get(index + 1) {
                Some(b's') => {
                    out.extend(quote(current.ok_or(NothingSelected)?));
                    index += 2;
                    continue;
                }
                Some(b'%') => {
                    out.push(b'%');
                    index += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    Ok(OsString::from_vec(out))
}

/// Name in single quotes for the shell. If it starts with `-`, it gets a leading `./` so the program
/// does not take it as an option.
pub fn quote(name: &OsStr) -> Vec<u8> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 4);
    out.push(b'\'');
    if bytes.starts_with(b"-") {
        out.extend_from_slice(b"./");
    }
    for &byte in bytes {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out
}

/// How a `!` command ended.
pub struct Exit {
    pub status: ExitStatus,
    /// How exporer stopped it, if it did.
    pub stopped: Option<Stop>,
}

/// How exporer stopped a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// SIGINT was sent to the group, like `Ctrl+C` in a terminal.
    Interrupted,
    /// SIGKILL was sent to the group.
    Forced,
}

/// Sends signals to a command's process group: the `sh` and everything it starts.
pub struct Control {
    group: libc::pid_t,
    state: Mutex<ControlState>,
}

#[derive(Default)]
struct ControlState {
    /// The `sh` was already reaped: its group number could belong to another process.
    reaped: bool,
    stopped: Option<Stop>,
}

impl Control {
    /// Sends SIGINT the first time and SIGKILL after that. Returns what it sent, or `None` if the
    /// command already ended.
    ///
    /// Processes the command left in the background with `&` stay in the group: they ignore
    /// SIGINT (a non-interactive `sh` starts them that way), but not SIGKILL.
    pub fn stop(&self) -> Option<Stop> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.reaped {
            return None;
        }
        let (stop, signal) = match state.stopped {
            None => (Stop::Interrupted, libc::SIGINT),
            Some(_) => (Stop::Forced, libc::SIGKILL),
        };
        // SAFETY: `killpg` only takes numbers. The group is this command's: its `sh` has not
        // been reaped yet, so the number cannot have passed to another process.
        if unsafe { libc::killpg(self.group, signal) } == -1 {
            return None;
        }
        state.stopped = Some(stop);
        Some(stop)
    }

    pub fn stopped(&self) -> Option<Stop> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped
    }
}

/// A running `!` command.
pub struct Process {
    child: Child,
    control: Arc<Control>,
    output: Arc<Output>,
    /// Completes when every process has closed the output.
    closed: mpsc::Receiver<()>,
}

/// Starts `sh -c command` in `cwd` with empty input, in a process group of its own so it can be
/// stopped as a whole. Does not block: [`Process::wait`] waits for the end.
///
/// The output is read on a thread of its own and cleaned as it arrives. `on_output` is called
/// from that thread when there is new output and the previous one was already taken with
/// [`Output::take`]: even if thousands of chunks arrive, the notice is not repeated until the UI
/// handles it.
pub fn spawn(
    command: &OsStr,
    cwd: &Path,
    on_output: impl Fn() + Send + Sync + 'static,
) -> io::Result<Process> {
    // A single pipe for both outputs keeps the order in which they are written.
    let (mut reader, writer) = io::pipe()?;
    let mut child = {
        let mut sh = Command::new("sh");
        sh.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(writer.try_clone()?)
            .stderr(writer);
        // Dropping `sh` closes this process's copies of the write end, so reading ends when the
        // command and the processes it started close it.
        sh.spawn()?
    };
    let control = Arc::new(Control {
        // With `process_group(0)` the group number is the pid of `sh`.
        group: child.id() as libc::pid_t,
        state: Mutex::default(),
    });

    let output = Arc::new(Output {
        state: Mutex::default(),
        on_output: Box::new(on_output),
    });
    let (closed_tx, closed_rx) = mpsc::channel();
    let shared = Arc::clone(&output);
    // Reading continues on its thread after `sh` ends while a background process keeps the
    // output open: closing it would send that process SIGPIPE.
    let spawned = thread::Builder::new()
        .name("exporer-cmd-output".to_owned())
        .spawn(move || {
            let mut chunk = [0; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => shared.push(&chunk[..read]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            closed_tx.send(()).ok();
        });
    if let Err(error) = spawned {
        // SAFETY: as in `Control::stop`; `sh` has not been reaped yet.
        unsafe { libc::killpg(control.group, libc::SIGKILL) };
        child.wait().ok();
        return Err(error);
    }
    Ok(Process {
        child,
        control,
        output,
        closed: closed_rx,
    })
}

impl Process {
    pub fn control(&self) -> Arc<Control> {
        Arc::clone(&self.control)
    }

    pub fn output(&self) -> Arc<Output> {
        Arc::clone(&self.output)
    }

    /// Waits for `sh` to end and closes the output: whatever arrives later is dropped. Blocks:
    /// meant for a thread of its own, not the UI thread.
    pub fn wait(mut self) -> io::Result<Exit> {
        // First wait without reaping: while `sh` is not reaped, its pid (which is the group
        // number) is not reused. It is marked as reaped before reaping it, under the same lock
        // `Control::stop` uses, so a foreign group is never signalled.
        let exited = wait_exited(self.child.id());
        let stopped = {
            let mut state = self
                .control
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.reaped = true;
            state.stopped
        };
        let status = self.child.wait()?;
        exited?;
        self.closed.recv_timeout(OUTPUT_GRACE).ok();
        self.output.finish();
        Ok(Exit { status, stopped })
    }
}

/// Waits for process `pid`, a child of this one, to end without reaping it.
fn wait_exited(pid: u32) -> io::Result<()> {
    loop {
        let mut info = mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `info` is valid memory for `waitid` to write to; with `WNOWAIT` the process is
        // left unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Starts `sh -c command` in `cwd` detached from exporer: in a new session, without a
/// controlling terminal and with input and outputs on `/dev/null`. That way it gets no `SIGHUP`
/// if the terminal exporer was started from closes, and no `SIGPIPE` when it writes after
/// exporer exits.
///
/// Does not block. A thread of its own waits for the program to end in order to reap it, and
/// then completes the returned receiver. If exporer exits first, the program is reparented and
/// keeps running.
pub fn launch(
    command: &OsStr,
    cwd: &Path,
) -> io::Result<oneshot::Receiver<io::Result<ExitStatus>>> {
    let (child_tx, child_rx) = mpsc::channel::<Child>();
    let (exit_tx, exit_rx) = oneshot::channel();
    // The thread is created before the process: if it cannot be created, no process is left
    // that nobody reaps. If the launch fails afterwards, `child_tx` is dropped and the thread
    // ends.
    thread::Builder::new()
        .name("exporer-launch".to_owned())
        .stack_size(LAUNCH_STACK)
        .spawn(move || {
            if let Ok(mut child) = child_rx.recv() {
                exit_tx.send(child.wait()).ok();
            }
        })?;
    let mut sh = Command::new("sh");
    sh.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: between `fork` and `exec` only `setsid` is called, which is async-signal-safe and
    // does not touch the process's memory.
    unsafe {
        sh.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = sh.spawn()?;
    // Only fails if the thread ended in a panic; the receiver is then cancelled.
    child_tx.send(child).ok();
    Ok(exit_rx)
}

/// Meaning of the codes the shell reserves, to report a program that did not start.
pub fn exit_reason(status: ExitStatus) -> Option<&'static str> {
    match status.code() {
        Some(127) => Some("command not found"),
        Some(126) => Some("not executable"),
        _ => None,
    }
}

/// Text for the exit status and whether it means a failure.
pub fn describe_status(status: ExitStatus) -> (String, bool) {
    if let Some(code) = status.code() {
        return (format!("exit {code}"), code != 0);
    }
    match status.signal() {
        Some(signal) => (format!("signal {signal}"), true),
        None => ("done".to_owned(), false),
    }
}

/// A command's output, cleaned and split into lines, shared between the thread that reads it and
/// the UI, which takes what is new with [`Output::take`]. Keeps the last `OUTPUT_LIMIT` bytes:
/// past it, the oldest lines are dropped.
pub struct Output {
    state: Mutex<OutputState>,
    on_output: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct OutputState {
    cleaner: Cleaner,
    /// Complete lines the UI has not taken yet.
    pending: VecDeque<SharedString>,
    /// Lines already taken that the UI has to drop from the start.
    dropped: usize,
    /// Size of each kept line, taken or not, from oldest to newest.
    sizes: VecDeque<usize>,
    bytes: usize,
    truncated: bool,
    /// New output was announced and has not been taken yet.
    notified: bool,
    /// The command ended: whatever arrives later is dropped.
    finished: bool,
}

/// What changed in the output since it was last taken.
#[derive(Debug, Default, PartialEq)]
pub struct OutputDelta {
    /// Lines to drop from the start of those already taken, because of the limit.
    pub dropped: usize,
    /// New complete lines.
    pub lines: Vec<SharedString>,
    /// The line being written, which goes after all the complete ones.
    pub open: Option<SharedString>,
    /// Something was already dropped because of the limit.
    pub truncated: bool,
}

impl Output {
    fn push(&self, chunk: &[u8]) {
        let notify = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.finished {
                return;
            }
            let mut lines = Vec::new();
            state.cleaner.feed(chunk, &mut |line| lines.push(line));
            for line in lines {
                state.keep(line);
            }
            !mem::replace(&mut state.notified, true)
        };
        if notify {
            (self.on_output)();
        }
    }

    /// The open line, if it received anything, becomes the last one.
    fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.finished {
            return;
        }
        let mut lines = Vec::new();
        state.cleaner.finish(&mut |line| lines.push(line));
        for line in lines {
            state.keep(line);
        }
        state.finished = true;
    }

    /// What is new since last time. After this, the next output announces itself again.
    pub fn take(&self) -> OutputDelta {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.notified = false;
        OutputDelta {
            dropped: mem::take(&mut state.dropped),
            lines: state.pending.drain(..).collect(),
            open: state.cleaner.open_line(),
            truncated: state.truncated || state.cleaner.truncated,
        }
    }
}

impl OutputState {
    fn keep(&mut self, line: SharedString) {
        let size = line.len() + 1;
        self.sizes.push_back(size);
        self.bytes += size;
        self.pending.push_back(line);
        while self.bytes > OUTPUT_LIMIT {
            let Some(size) = self.sizes.pop_front() else {
                break;
            };
            self.bytes -= size;
            self.truncated = true;
            // Pending lines are the newest: if taken ones remain, the oldest is one of them.
            if self.sizes.len() + 1 > self.pending.len() {
                self.dropped += 1;
            } else {
                self.pending.pop_front();
            }
        }
    }
}

/// State inside an escape sequence.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Escape {
    #[default]
    None,
    /// After `ESC`.
    Start,
    /// `ESC [ …` up to the final byte.
    Csi,
    /// Strings such as OSC (`ESC ] …`), up to `BEL` or `ESC \`.
    String,
    /// `ESC` inside a string.
    StringEsc,
    /// Short sequences such as `ESC ( B`.
    Intermediate,
}

/// Splits output into lines to show it as plain text, as it arrives in chunks of any size:
/// without escape sequences (colors), `\r` keeps what was last written on the line, backspace
/// deletes the previous character and tabs are expanded. A UTF-8 character or an escape sequence
/// split across two chunks is handled as if it were whole.
#[derive(Default)]
pub struct Cleaner {
    /// The line being written.
    line: Vec<char>,
    /// Something arrived after the last line break: at the end, it is one more line.
    touched: bool,
    /// Bytes of a character left incomplete at the end of the previous chunk.
    partial: Vec<u8>,
    escape: Escape,
    /// `\r` arrived: the line is cleared by the next character, unless it is `\n`.
    carriage: bool,
    /// The first line is empty and nothing else arrived. On its own it is not output (`echo`); if
    /// something arrives, it is delivered first.
    held_empty: bool,
    /// Some line was already delivered.
    started: bool,
    /// A line went past `OUTPUT_LIMIT` characters and was cut.
    truncated: bool,
}

impl Cleaner {
    /// Processes a chunk and hands the lines it completed to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut impl FnMut(SharedString)) {
        let joined;
        let mut rest = if self.partial.is_empty() {
            bytes
        } else {
            let mut partial = mem::take(&mut self.partial);
            partial.extend_from_slice(bytes);
            joined = partial;
            &joined[..]
        };
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.push_str(text, out);
                    return;
                }
                Err(error) => {
                    let (valid, invalid) = rest.split_at(error.valid_up_to());
                    // SAFETY: `from_utf8` just validated these bytes.
                    self.push_str(unsafe { std::str::from_utf8_unchecked(valid) }, out);
                    match error.error_len() {
                        // Like `String::from_utf8_lossy`.
                        Some(length) => {
                            self.push_char(char::REPLACEMENT_CHARACTER, out);
                            rest = &invalid[length..];
                        }
                        None => {
                            self.partial = invalid.to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Ends the output: the open line, if it received anything, is the last one.
    pub fn finish(&mut self, out: &mut impl FnMut(SharedString)) {
        if !self.partial.is_empty() {
            self.partial.clear();
            self.push_char(char::REPLACEMENT_CHARACTER, out);
        }
        if self.touched {
            self.close_line(out);
        }
        self.held_empty = false;
    }

    /// What shows of the line being written, if it received anything.
    pub fn open_line(&self) -> Option<SharedString> {
        self.touched
            .then(|| self.line.iter().collect::<String>().into())
    }

    fn push_str(&mut self, text: &str, out: &mut impl FnMut(SharedString)) {
        for c in text.chars() {
            self.push_char(c, out);
        }
    }

    fn push_char(&mut self, c: char, out: &mut impl FnMut(SharedString)) {
        // An escape sequence does not continue on the next line.
        if c == '\n' {
            self.escape = Escape::None;
            self.carriage = false;
            self.close_line(out);
            return;
        }
        self.touched = true;
        self.release_held(out);
        match self.escape {
            Escape::None => {}
            Escape::Start => {
                self.escape = match c {
                    '[' => Escape::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => Escape::String,
                    '\u{20}'..='\u{2f}' => Escape::Intermediate,
                    _ => Escape::None,
                };
                return;
            }
            Escape::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.escape = Escape::None;
                }
                return;
            }
            Escape::String => {
                match c {
                    '\u{7}' => self.escape = Escape::None,
                    '\u{1b}' => self.escape = Escape::StringEsc,
                    _ => {}
                }
                return;
            }
            Escape::StringEsc => {
                self.escape = Escape::None;
                // `ESC \` ends the string. If something else follows, it is processed as text.
                if c == '\\' {
                    return;
                }
            }
            Escape::Intermediate => {
                if !('\u{20}'..='\u{2f}').contains(&c) {
                    self.escape = Escape::None;
                }
                return;
            }
        }
        if mem::take(&mut self.carriage) {
            self.line.clear();
        }
        match c {
            '\u{1b}' => self.escape = Escape::Start,
            '\r' => self.carriage = true,
            '\u{8}' => {
                self.line.pop();
            }
            '\t' => {
                let spaces = TAB_WIDTH - self.line.len() % TAB_WIDTH;
                for _ in 0..spaces {
                    self.push_visible(' ');
                }
            }
            c if c.is_control() => {}
            c => self.push_visible(c),
        }
    }

    fn push_visible(&mut self, c: char) {
        if self.line.len() < OUTPUT_LIMIT {
            self.line.push(c);
        } else {
            self.truncated = true;
        }
    }

    fn close_line(&mut self, out: &mut impl FnMut(SharedString)) {
        let line: String = self.line.drain(..).collect();
        self.touched = false;
        if !self.started && !self.held_empty && line.is_empty() {
            self.held_empty = true;
            return;
        }
        self.release_held(out);
        self.started = true;
        out(line.into());
    }

    fn release_held(&mut self, out: &mut impl FnMut(SharedString)) {
        if mem::take(&mut self.held_empty) {
            self.started = true;
            out(SharedString::default());
        }
    }
}

/// Commands run in the session, from oldest to newest.
#[derive(Default)]
pub struct History {
    items: Vec<String>,
    /// Position while walking with `↑`/`↓`; `None` while typing.
    index: Option<usize>,
    /// What was being typed before walking started.
    draft: String,
}

impl History {
    /// Adds a command that was run, unless it is empty or repeats the last one.
    pub fn push(&mut self, command: &str) {
        self.reset();
        if command.trim().is_empty() || self.items.last().is_some_and(|last| last == command) {
            return;
        }
        if self.items.len() == HISTORY_LIMIT {
            self.items.remove(0);
        }
        self.items.push(command.to_owned());
    }

    /// Goes back to the typing position, with no draft.
    pub fn reset(&mut self) {
        self.index = None;
        self.draft.clear();
    }

    /// The command before the one shown. `current` is the field's text: it is saved when walking
    /// starts so `newer` can return to it. `None` if there is no earlier one.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let index = match self.index {
            None => {
                let last = self.items.len().checked_sub(1)?;
                self.draft = current.to_owned();
                last
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.index = Some(index);
        Some(&self.items[index])
    }

    /// The command after the one shown or, past the newest, what was being typed. `None` if not
    /// walking.
    pub fn newer(&mut self) -> Option<&str> {
        let index = self.index?;
        if index + 1 < self.items.len() {
            self.index = Some(index + 1);
            Some(&self.items[index + 1])
        } else {
            self.index = None;
            Some(&self.draft)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::iter::Peekable;
    use std::ops::Range;
    use std::str::Chars;
    use std::time::Instant;

    fn expanded(command: &str, current: Option<&str>) -> Result<String, NothingSelected> {
        expand(command, current.map(OsStr::new))
            .map(|command| command.into_string().expect("UTF-8 in the test"))
    }

    /// The previous cleaning, unchanged, over the whole text at once: the reference that `Cleaner`
    /// has to reproduce even when bytes arrive split.
    fn reference_lines(text: &str) -> Vec<SharedString> {
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.is_empty() {
            return Vec::new();
        }
        text.split('\n')
            .map(|line| SharedString::from(clean_line(line)))
            .collect()
    }

    fn clean_line(line: &str) -> String {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let mut out: Vec<char> = Vec::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => skip_escape(&mut chars),
                '\r' => out.clear(),
                '\u{8}' => {
                    out.pop();
                }
                '\t' => {
                    let spaces = TAB_WIDTH - out.len() % TAB_WIDTH;
                    out.extend(std::iter::repeat_n(' ', spaces));
                }
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        out.into_iter().collect()
    }

    /// Skips an escape sequence after `ESC`: CSI (`ESC [ … final`), strings such as OSC
    /// (`ESC ] … BEL` or `ESC ] … ESC \`) and short sequences (`ESC ( B`, `ESC =`).
    fn skip_escape(chars: &mut Peekable<Chars>) {
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                for c in chars.by_ref() {
                    if !('\u{20}'..='\u{2f}').contains(&c) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    /// Lines from `Cleaner` with the bytes in the given chunks.
    fn cleaned(chunks: &[&[u8]]) -> Vec<String> {
        let mut cleaner = Cleaner::default();
        let mut lines = Vec::new();
        for chunk in chunks {
            cleaner.feed(chunk, &mut |line| lines.push(line.to_string()));
        }
        cleaner.finish(&mut |line| lines.push(line.to_string()));
        lines
    }

    fn lines(text: &str) -> Vec<String> {
        cleaned(&[text.as_bytes()])
    }

    struct Finished {
        status: ExitStatus,
        lines: Vec<SharedString>,
        truncated: bool,
        stopped: Option<Stop>,
    }

    /// Waits for the end and takes all the output, which must be complete.
    fn collect(process: Process) -> io::Result<Finished> {
        let output = process.output();
        let exit = process.wait()?;
        let delta = output.take();
        assert_eq!(delta.dropped, 0);
        assert_eq!(delta.open, None);
        Ok(Finished {
            status: exit.status,
            lines: delta.lines,
            truncated: delta.truncated,
            stopped: exit.stopped,
        })
    }

    fn run(command: &OsStr, cwd: &Path) -> io::Result<Finished> {
        collect(spawn(command, cwd, || {})?)
    }

    fn run_ok(command: &str) -> Finished {
        run(OsStr::new(command), Path::new("/")).expect("sh should run")
    }

    #[test]
    fn expands_placeholders() {
        assert_eq!(expanded("ls -la", None), Ok("ls -la".to_owned()));
        assert_eq!(
            expanded("cat %s", Some("a.txt")),
            Ok("cat 'a.txt'".to_owned())
        );
        assert_eq!(
            expanded("cp %s %s.bak", Some("my photo.png")),
            Ok("cp 'my photo.png' 'my photo.png'.bak".to_owned())
        );
        assert_eq!(expanded("date +%%s", None), Ok("date +%s".to_owned()));
        assert_eq!(
            expanded("printf '%d' 5 %", None),
            Ok("printf '%d' 5 %".to_owned())
        );
        assert_eq!(expanded("ls %%%s", Some("x")), Ok("ls %'x'".to_owned()));
        assert_eq!(expanded("cd %s", Some("..")), Ok("cd '..'".to_owned()));
        assert_eq!(expanded("cat %s", None), Err(NothingSelected));
        // Without `%s`, no entry is needed.
        assert_eq!(
            expanded("echo naïve café", None),
            Ok("echo naïve café".to_owned())
        );
    }

    #[test]
    fn quotes_names_for_the_shell() {
        assert_eq!(quote(OsStr::new("simple")), b"'simple'");
        assert_eq!(quote(OsStr::new("it's")), b"'it'\\''s'");
        assert_eq!(quote(OsStr::new("-rf")), b"'./-rf'");
        assert_eq!(quote(OsStr::new("$(rm x) `y`")), b"'$(rm x) `y`'");
        let invalid = OsStr::from_bytes(b"caf\xe9");
        assert_eq!(quote(invalid), b"'caf\xe9'");
    }

    #[test]
    fn quoted_names_reach_the_command_intact() {
        for name in ["it's", "-rf", "$(echo no)", "a  b", "\"x\"", "tab\there"] {
            let command = expand("printf '%%s\\n' %s", Some(OsStr::new(name))).unwrap();
            let finished = run(&command, Path::new("/")).unwrap();
            let expected = if name.starts_with('-') {
                format!("./{name}")
            } else {
                name.to_owned()
            };
            assert_eq!(finished.lines, lines(&expected), "{name}");
        }
    }

    #[test]
    fn cleans_output_lines() {
        assert_eq!(lines(""), Vec::<String>::new());
        assert_eq!(lines("\n"), Vec::<String>::new());
        assert_eq!(lines("one\ntwo\n"), vec!["one", "two"]);
        assert_eq!(lines("one\n\ntwo"), vec!["one", "", "two"]);
        assert_eq!(lines("crlf\r\n"), vec!["crlf"]);
        assert_eq!(lines("10%\r50%\r100%\n"), vec!["100%"]);
        assert_eq!(lines("\u{1b}[1;31mred\u{1b}[0m normal"), vec!["red normal"]);
        assert_eq!(lines("\u{1b}]0;café\u{7}text"), vec!["text"]);
        assert_eq!(lines("\u{1b}]8;;http://x\u{1b}\\link"), vec!["link"]);
        assert_eq!(lines("\u{1b}(Bend\u{1b}="), vec!["end"]);
        assert_eq!(lines("a\tb\n1234567\tc"), vec!["a       b", "1234567 c"]);
        assert_eq!(lines("B\u{8}Bo\u{8}ol\u{8}ld\u{8}d"), vec!["Bold"]);
        assert_eq!(lines("bell\u{7} nul\u{0}"), vec!["bell nul"]);
    }

    /// Cases from `cleans_output_lines` and others with multi-byte characters and invalid bytes.
    const CLEANING_CASES: &[&[u8]] = &[
        b"",
        b"\n",
        b"\n\n",
        b"one\ntwo\n",
        b"one\n\ntwo",
        b"crlf\r\n",
        b"10%\r50%\r100%\n",
        b"bar\r\x1b[Kdone\n",
        b"\x1b[1;31mred\x1b[0m normal",
        b"\x1b]0;caf\xc3\xa9\x07text",
        b"\x1b]8;;http://x\x1b\\link",
        b"\x1b]2;unterminated\nnext",
        b"\x1b(Bend\x1b=",
        b"a\tb\n1234567\tc",
        b"B\x08Bo\x08ol\x08ld\x08d",
        b"bell\x07 nul\x00",
        b"\xc3\xb1and\xc3\xba \xe6\x97\xa5\xe6\x9c\xac\xf0\x9f\x98\x80\n",
        b"caf\xe9 in\xc3valid\xff\nend\xe2\x82",
        b"\nempty first",
        b"\n\x1b[0m",
    ];

    #[test]
    fn incremental_cleaning_matches_the_whole_text() {
        for case in CLEANING_CASES {
            let expected: Vec<String> = reference_lines(&String::from_utf8_lossy(case))
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(cleaned(&[case]), expected, "{case:?}");
            for split in 0..=case.len() {
                let (a, b) = case.split_at(split);
                assert_eq!(cleaned(&[a, b]), expected, "{case:?} split at {split}");
            }
            let bytes: Vec<&[u8]> = case.chunks(1).collect();
            assert_eq!(cleaned(&bytes), expected, "{case:?} byte by byte");
        }
    }

    #[test]
    fn open_line_shows_what_is_being_written() {
        let mut cleaner = Cleaner::default();
        let mut lines = Vec::new();
        let mut feed = |bytes: &[u8]| {
            cleaner.feed(bytes, &mut |line| lines.push(line.to_string()));
            cleaner.open_line().map(|line| line.to_string())
        };
        assert_eq!(feed(b""), None);
        assert_eq!(feed(b"10%"), Some("10%".to_owned()));
        // After `\r`, the previous text still shows until the new one arrives.
        assert_eq!(feed(b"\r"), Some("10%".to_owned()));
        assert_eq!(feed(b"50"), Some("50".to_owned()));
        assert_eq!(feed(b"%\x1b[3"), Some("50%".to_owned()));
        assert_eq!(feed(b"1mx\n"), None);
        assert_eq!(feed(b"\xc3"), None);
        assert_eq!(feed(b"\xb1"), Some("ñ".to_owned()));
        assert_eq!(lines, vec!["50%x"]);
    }

    fn output_counting() -> (Output, Arc<std::sync::atomic::AtomicUsize>) {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let output = Output {
            state: Mutex::default(),
            on_output: Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        };
        (output, count)
    }

    #[test]
    fn output_notifies_once_until_taken() {
        let (output, count) = output_counting();
        let notified = || count.load(std::sync::atomic::Ordering::SeqCst);
        output.push(b"one\n");
        output.push(b"two\nthr");
        assert_eq!(notified(), 1);
        let delta = output.take();
        assert_eq!(delta.lines, vec!["one", "two"]);
        assert_eq!(delta.open, Some("thr".into()));
        output.push(b"ee\n");
        assert_eq!(notified(), 2);
        output.finish();
        output.push(b"late\n");
        assert_eq!(notified(), 2, "dropped after the end");
        let delta = output.take();
        assert_eq!(delta.lines, vec!["three"]);
        assert_eq!(delta.open, None);
    }

    #[test]
    fn output_keeps_the_last_mebibyte() {
        let (output, _) = output_counting();
        // 1 KiB lines including the line break: exactly 1024 fit.
        let line = |index: usize| format!("{index:>1023}\n");
        let lines = |range: Range<usize>| range.map(line).collect::<String>();
        output.push(lines(0..1024).as_bytes());
        let delta = output.take();
        assert_eq!(
            (delta.dropped, delta.lines.len(), delta.truncated),
            (0, 1024, false)
        );

        output.push(lines(1024..1034).as_bytes());
        let delta = output.take();
        assert_eq!(
            (delta.dropped, delta.lines.len(), delta.truncated),
            (10, 10, true)
        );
        assert_eq!(delta.lines[0].trim(), "1024");

        // Without taking in between: all taken lines and the oldest pending ones are dropped.
        output.push(lines(1034..4034).as_bytes());
        let delta = output.take();
        assert_eq!((delta.dropped, delta.lines.len()), (1024, 1024));
        assert_eq!(delta.lines[0].trim(), "3010");
        assert_eq!(delta.lines[1023].trim(), "4033");
    }

    #[test]
    fn output_arrives_while_the_command_runs() {
        let (tx, rx) = mpsc::channel();
        let process = spawn(
            OsStr::new("printf a; sleep 0.5; echo b; sleep 0.5; echo c"),
            Path::new("/"),
            move || tx.send(()).unwrap(),
        )
        .unwrap();
        let output = process.output();
        let finished = thread::spawn(move || process.wait().unwrap());
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let delta = output.take();
        assert!(delta.lines.is_empty());
        assert_eq!(delta.open, Some("a".into()));
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let delta = output.take();
        assert_eq!(delta.lines, vec!["ab"]);
        assert!(finished.join().unwrap().status.success());
        let delta = output.take();
        assert_eq!(delta.lines, vec!["c"]);
        assert_eq!(delta.open, None);
    }

    #[test]
    fn describes_exit_status() {
        assert_eq!(
            describe_status(ExitStatus::from_raw(0)),
            ("exit 0".to_owned(), false)
        );
        assert_eq!(
            describe_status(ExitStatus::from_raw(3 << 8)),
            ("exit 3".to_owned(), true)
        );
        assert_eq!(
            describe_status(ExitStatus::from_raw(9)),
            ("signal 9".to_owned(), true)
        );
    }

    #[test]
    fn walks_history() {
        let mut history = History::default();
        assert_eq!(history.older("draft"), None);
        assert_eq!(history.newer(), None);

        history.push("one");
        history.push("two");
        history.push("two");
        history.push("   ");
        history.push("three");

        assert_eq!(history.older("typing"), Some("three"));
        assert_eq!(history.older("three"), Some("two"));
        assert_eq!(history.older("two"), Some("one"));
        assert_eq!(history.older("one"), None);
        assert_eq!(history.newer(), Some("two"));
        assert_eq!(history.newer(), Some("three"));
        assert_eq!(history.newer(), Some("typing"));
        assert_eq!(history.newer(), None);

        // Running goes back to the typing position.
        history.older("");
        history.push("four");
        assert_eq!(history.older(""), Some("four"));

        let mut full = History::default();
        for index in 0..=HISTORY_LIMIT {
            full.push(&index.to_string());
        }
        assert_eq!(full.items.len(), HISTORY_LIMIT);
        assert_eq!(full.items[0], "1");
    }

    #[test]
    fn runs_commands_with_combined_output() {
        let finished = run_ok("echo one; echo two >&2; echo three");
        assert_eq!(finished.lines, vec!["one", "two", "three"]);
        assert!(finished.status.success());
        assert!(!finished.truncated);

        assert_eq!(run_ok("exit 3").status.code(), Some(3));
        assert_eq!(run_ok("kill -9 $$").status.signal(), Some(9));
        // Input is empty: `cat` does not wait.
        assert!(run_ok("cat").lines.is_empty());
    }

    #[test]
    fn runs_in_the_given_folder() {
        let dir = std::env::temp_dir().join(format!("exporer-shell-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let finished = run(OsStr::new("pwd; touch created"), &dir).unwrap();
        assert_eq!(finished.lines, vec![dir.display().to_string()]);
        assert!(dir.join("created").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn background_processes_do_not_hold_the_result() {
        let start = Instant::now();
        let finished = run_ok("sleep 3 & echo launched");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(finished.lines, vec!["launched"]);
        assert!(finished.status.success());
    }

    #[test]
    fn truncates_long_output() {
        let finished = run_ok("yes | head -c 3000000");
        assert!(finished.truncated);
        assert!(finished.status.success());
        assert_eq!(finished.lines.len(), OUTPUT_LIMIT / 2);
    }

    #[test]
    fn missing_folder_is_an_error() {
        assert!(run(OsStr::new("true"), Path::new("/does/not/exist/exporer")).is_err());
        assert!(launch(OsStr::new("true"), Path::new("/does/not/exist/exporer")).is_err());
    }

    /// Starts `command` and waits for its result on another thread, so it can be stopped meanwhile.
    fn spawn_waiting(command: &str) -> (Arc<Control>, mpsc::Receiver<Finished>) {
        let process = spawn(OsStr::new(command), Path::new("/"), || {}).unwrap();
        let control = process.control();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(collect(process).unwrap()).unwrap());
        // Let `sh` start the command before stopping it.
        thread::sleep(Duration::from_millis(300));
        (control, rx)
    }

    fn group_is_empty(group: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks whether any process exists in the group.
        unsafe { libc::killpg(group, 0) == -1 }
    }

    #[test]
    fn interrupt_stops_the_whole_group() {
        let start = Instant::now();
        let (control, finished) = spawn_waiting("sleep 30 | cat; echo never-printed");
        assert_eq!(control.stop(), Some(Stop::Interrupted));
        let finished = finished.recv().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(finished.status.signal(), Some(libc::SIGINT));
        assert_eq!(finished.stopped, Some(Stop::Interrupted));
        assert!(finished.lines.is_empty());
        assert!(group_is_empty(control.group));
        // Ended and reaped: nothing is sent to it anymore.
        assert_eq!(control.stop(), None);
    }

    #[test]
    fn a_second_stop_forces_what_ignores_the_interrupt() {
        let (control, finished) = spawn_waiting("trap '' INT; sleep 30");
        assert_eq!(control.stop(), Some(Stop::Interrupted));
        assert!(
            finished.recv_timeout(Duration::from_millis(500)).is_err(),
            "it ignores SIGINT and keeps running"
        );
        assert_eq!(control.stopped(), Some(Stop::Interrupted));
        assert_eq!(control.stop(), Some(Stop::Forced));
        let finished = finished.recv().unwrap();
        assert_eq!(finished.status.signal(), Some(libc::SIGKILL));
        assert_eq!(finished.stopped, Some(Stop::Forced));
        assert!(group_is_empty(control.group));
    }

    #[test]
    fn background_jobs_survive_an_interrupt() {
        let dir = std::env::temp_dir().join(format!("exporer-stop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let command = format!("sleep 31 & echo $! > '{}'; sleep 32", pid_file.display());
        let (control, finished) = spawn_waiting(&command);
        assert_eq!(control.stop(), Some(Stop::Interrupted));
        assert_eq!(finished.recv().unwrap().stopped, Some(Stop::Interrupted));
        let pid: libc::pid_t = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signals to a process of this test; signal 0 only checks that it exists.
        unsafe {
            assert_eq!(libc::kill(pid, 0), 0, "the `&` process is still alive");
            libc::kill(pid, libc::SIGKILL);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finished_commands_are_not_signalled() {
        let process = spawn(OsStr::new("true"), Path::new("/"), || {}).unwrap();
        let control = process.control();
        let finished = collect(process).unwrap();
        assert_eq!(finished.stopped, None);
        assert_eq!(control.stop(), None);
        assert_eq!(control.stopped(), None);
    }

    fn wait_launched(exited: oneshot::Receiver<io::Result<ExitStatus>>) -> ExitStatus {
        futures::executor::block_on(exited)
            .expect("the waiting thread answers")
            .expect("wait")
    }

    #[test]
    fn launches_detached_from_exporer() {
        let dir = std::env::temp_dir().join(format!("exporer-launch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Fields 6 and 7 of /proc/<pid>/stat: session and controlling terminal (0 if none).
        let command = "echo $$ $(cut -d' ' -f6,7 /proc/$$/stat) > info; \
                       readlink /proc/$$/fd/0 /proc/$$/fd/1 /proc/$$/fd/2 > fds";
        assert!(wait_launched(launch(OsStr::new(command), &dir).unwrap()).success());

        let info = fs::read_to_string(dir.join("info")).unwrap();
        let fields: Vec<&str> = info.split_whitespace().collect();
        // SAFETY: `getsid` only reads this process's session id.
        let own_session = unsafe { libc::getsid(0) }.to_string();
        assert_eq!(fields[0], fields[1], "sh leads its own session");
        assert_ne!(fields[1], own_session);
        assert_eq!(fields[2], "0", "no controlling terminal");
        assert_eq!(
            fs::read_to_string(dir.join("fds")).unwrap(),
            "/dev/null\n/dev/null\n/dev/null\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launch_reports_how_the_program_ended() {
        let status = wait_launched(launch(OsStr::new("exit 3"), Path::new("/")).unwrap());
        assert_eq!(status.code(), Some(3));
        assert_eq!(exit_reason(status), None);
        let missing = launch(OsStr::new("missing-exporer-xyz ."), Path::new("/")).unwrap();
        let status = wait_launched(missing);
        assert_eq!(status.code(), Some(127));
        assert_eq!(exit_reason(status), Some("command not found"));
        assert_eq!(
            exit_reason(ExitStatus::from_raw(126 << 8)),
            Some("not executable")
        );
    }

    #[test]
    fn launch_does_not_wait_for_the_program() {
        let start = Instant::now();
        let mut exited = launch(OsStr::new("sleep 1"), Path::new("/")).unwrap();
        assert!(start.elapsed() < LAUNCH_GRACE, "{:?}", start.elapsed());
        thread::sleep(Duration::from_millis(300));
        assert!(matches!(exited.try_recv(), Ok(None)), "still running");
        assert!(wait_launched(exited).success());
        assert!(start.elapsed() >= Duration::from_secs(1));
    }
}

//! Main view: path bar, list, preview and status bar, on a single background and separated by
//! 1 px lines.
//!
//! It has four modes. In `Normal` the keyboard navigates the list; `i` switches to `Insert`,
//! which edits the path in the top bar, `!` or `&` to `Command`, which types a shell command in
//! the status bar, and `/` to `Search`, which moves the cursor to the entries that match.
//! `Space` opens a quick look over the list with a large preview of the entry under the cursor.
//! `Tab`, `Ctrl+A` and the visual selection (`v`) mark entries, by full path, for the operations
//! on files: `yy` copies and `xx` cuts them into a register that `pp` pastes in the shown folder.
//! `Escape` or losing focus go back to `Normal`. The output of `!` commands
//! shows in a panel between the list and the status bar; `&` commands are launched detached,
//! without output.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::{self, Either};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, Focusable,
    FontWeight, HighlightStyle, Hsla, ImageSource, KeyBinding, ListHorizontalSizingBehavior,
    ObjectFit, ScrollStrategy, SharedString, Stateful, StyledImage, StyledText, Subscription, Svg,
    Task, Transformation, UniformListScrollHandle, Window, actions, div, img, prelude::*, px,
    radians, relative, svg, transparent_black, uniform_list,
};

use crate::assets::{MONO_FONT, UI_FONT, icon};
use crate::complete;
use crate::fileops::{self, Register, RegisterStore};
use crate::listing::{self, Entry};
use crate::preview;
use crate::search::{self, Direction};
use crate::selection::{self, Marks};
use crate::shell;
use crate::text_input::{self, TextInput};
use crate::theme::Theme;

actions!(
    explorer,
    [
        MoveDown,
        MoveUp,
        Open,
        Parent,
        First,
        Last,
        EditPath,
        ConfirmPath,
        CompleteNext,
        CompletePrevious,
        CancelEdit,
        EnterCommand,
        EnterLaunch,
        EnterSearch,
        ConfirmSearch,
        NextMatch,
        PreviousMatch,
        ToggleQuickLook,
        ToggleMark,
        ToggleVisual,
        ExtendDown,
        ExtendUp,
        MarkAll,
        TrashEntries,
        DeleteEntries,
        ConfirmDelete,
        Yank,
        Cut,
        Paste,
        RunCommand,
        HistoryOlder,
        HistoryNewer,
        CloseOutput,
        Interrupt,
        Quit
    ]
);

/// Keyboard context of the list. It covers the list, the preview and the output panel, but not
/// the bars: that way, while the path or a command is being edited, `j`, `h`, `Enter`… do not
/// reach the list.
const LIST_CONTEXT: &str = "FileList";
/// Context of the top bar while the path is being edited.
const PATH_BAR_CONTEXT: &str = "PathBar";
/// Context of the status bar while a command is being typed.
const COMMAND_LINE_CONTEXT: &str = "CommandLine";
/// Context of the status bar while a search is being typed.
const SEARCH_LINE_CONTEXT: &str = "SearchLine";
/// Context of the confirmation dialog before moving to trash or deleting for good.
const CONFIRM_CONTEXT: &str = "Confirm";
/// Names shown when asking before deleting; the rest are counted.
const CONFIRM_NAMES: usize = 3;
const ROW_HEIGHT: f32 = 32.;
/// Height of the column names above the list and of the preview panel's title, which share the
/// same dividing line.
const SECTION_HEADER_HEIGHT: f32 = 26.;
/// Height of each line and of the output panel header.
const OUTPUT_ROW_HEIGHT: f32 = 18.;
const OUTPUT_HEADER_HEIGHT: f32 = 32.;
/// The output panel takes at most this fraction of the window height.
const OUTPUT_MAX_FRACTION: f32 = 0.4;
/// A command that ends before this delay never shows its row, so nothing flickers.
const JOB_ROW_DELAY: Duration = Duration::from_millis(250);
/// How often the spinner advances (by 45°) and the rows' elapsed time updates.
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const JOB_ROW_HEIGHT: f32 = 30.;
/// The output of a running command is taken at most this often.
const LIVE_INTERVAL: Duration = Duration::from_millis(50);
/// If a take brought more than `FLOOD_LINES` lines (`yes`, `cat` of a huge file), the next one
/// waits `FLOOD_INTERVAL`: every take has a cost and the UI has to stay responsive.
const FLOOD_LINES: usize = 10_000;
const FLOOD_INTERVAL: Duration = Duration::from_millis(250);
/// Command rows visible at once; the rest are summarized in one line.
const MAX_JOB_ROWS: usize = 5;
/// How long the notice of a command that ended without printing anything stays visible.
const NOTICE_DURATION: Duration = Duration::from_secs(5);
/// The image preview waits this long without the cursor moving before decoding, so holding `j`
/// does not decode every image on the way.
const PREVIEW_DELAY: Duration = Duration::from_millis(60);
/// Image thumbnails kept in memory.
const PREVIEW_CACHE_SIZE: usize = 16;
/// Height limits of the image frame in the preview panel; the frame follows the image's
/// proportion in between.
const PREVIEW_FRAME_MIN: f32 = 160.;
const PREVIEW_FRAME_MAX: f32 = 420.;
/// Width available for the image inside the frame: panel (360) minus padding (2 × 14), border
/// (2 × 1) and the frame's own padding (2 × 8).
const PREVIEW_IMAGE_WIDTH: f32 = 314.;
/// Below this window width the preview is hidden to leave room for the list.
const PREVIEW_MIN_WINDOW_WIDTH: f32 = 864.;
/// Approximate width of a JetBrains Mono character at 16 px, used to shorten the path.
const PATH_CHAR_WIDTH: f32 = 9.6;
/// Height of the path bar.
const HEADER_HEIGHT: f32 = 40.;
/// Completions visible at once in the list under the path bar.
const COMPLETION_ROWS: usize = 10;
const COMPLETION_ROW_HEIGHT: f32 = 28.;

/// The completions offered for the typed path while there are several.
struct Completion {
    /// The folder part of the text, as typed; the chosen candidate goes after it.
    dir_text: String,
    candidates: Vec<complete::Candidate>,
    /// The candidate shown in the field; `None` until `Tab` goes through the list.
    selected: Option<usize>,
}

pub fn bind_keys(cx: &mut App) {
    let list = Some(LIST_CONTEXT);
    let path_bar = Some(PATH_BAR_CONTEXT);
    let command_line = Some(COMMAND_LINE_CONTEXT);
    let search_line = Some(SEARCH_LINE_CONTEXT);
    let confirm = Some(CONFIRM_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("j", MoveDown, list),
        KeyBinding::new("down", MoveDown, list),
        KeyBinding::new("k", MoveUp, list),
        KeyBinding::new("up", MoveUp, list),
        KeyBinding::new("l", Open, list),
        KeyBinding::new("right", Open, list),
        KeyBinding::new("enter", Open, list),
        KeyBinding::new("h", Parent, list),
        KeyBinding::new("left", Parent, list),
        KeyBinding::new("backspace", Parent, list),
        KeyBinding::new("g g", First, list),
        KeyBinding::new("home", First, list),
        KeyBinding::new("shift-g", Last, list),
        KeyBinding::new("end", Last, list),
        KeyBinding::new("i", EditPath, list),
        // On Linux, gpui delivers `Shift+1` as the `!` key with no modifier.
        KeyBinding::new("!", EnterCommand, list),
        KeyBinding::new("&", EnterLaunch, list),
        KeyBinding::new("/", EnterSearch, list),
        KeyBinding::new("n", NextMatch, list),
        KeyBinding::new("shift-n", PreviousMatch, list),
        KeyBinding::new("space", ToggleQuickLook, list),
        KeyBinding::new("tab", ToggleMark, list),
        KeyBinding::new("v", ToggleVisual, list),
        KeyBinding::new("ctrl-a", MarkAll, list),
        KeyBinding::new("y y", Yank, list),
        KeyBinding::new("x x", Cut, list),
        KeyBinding::new("p p", Paste, list),
        KeyBinding::new("ctrl-x", Cut, list),
        KeyBinding::new("ctrl-v", Paste, list),
        KeyBinding::new("shift-down", ExtendDown, list),
        KeyBinding::new("shift-up", ExtendUp, list),
        KeyBinding::new("d d", TrashEntries, list),
        KeyBinding::new("shift-d", DeleteEntries, list),
        KeyBinding::new("y", ConfirmDelete, confirm),
        KeyBinding::new("enter", ConfirmDelete, confirm),
        KeyBinding::new("n", CancelEdit, confirm),
        KeyBinding::new("escape", CancelEdit, confirm),
        KeyBinding::new("escape", CloseOutput, list),
        // Interrupts what is running in view, or copies. In text fields `ctrl-c` is still copy:
        // this context does not cover them.
        KeyBinding::new("ctrl-c", Interrupt, list),
        KeyBinding::new("enter", ConfirmPath, path_bar),
        KeyBinding::new("escape", CancelEdit, path_bar),
        KeyBinding::new("tab", CompleteNext, path_bar),
        KeyBinding::new("down", CompleteNext, path_bar),
        KeyBinding::new("shift-tab", CompletePrevious, path_bar),
        KeyBinding::new("up", CompletePrevious, path_bar),
        KeyBinding::new("enter", RunCommand, command_line),
        KeyBinding::new("escape", CancelEdit, command_line),
        KeyBinding::new("up", HistoryOlder, command_line),
        KeyBinding::new("down", HistoryNewer, command_line),
        KeyBinding::new("enter", ConfirmSearch, search_line),
        KeyBinding::new("escape", CancelEdit, search_line),
        KeyBinding::new("up", HistoryOlder, search_line),
        KeyBinding::new("down", HistoryNewer, search_line),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Folder navigation.
    Normal,
    /// Editing the path in the top bar.
    Insert,
    /// Typing a shell command in the status bar.
    Command(CommandKind),
    /// Typing a search in the status bar.
    Search,
    /// Asking in the status bar before deleting for good (`D`).
    Confirm,
}

/// What to do with the typed command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    /// `!`: run it and show its output.
    Run,
    /// `&`: launch it detached from exporer, without output.
    Launch,
}

impl CommandKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Run => "!",
            Self::Launch => "&",
        }
    }
}

/// A `!` command: running, or ended with output that can still be viewed.
struct Job {
    id: u64,
    /// The command that was run, with `%s` already substituted.
    command: SharedString,
    control: Arc<shell::Control>,
    output: Arc<shell::Output>,
    started: Instant,
    /// Its row is visible: `JOB_ROW_DELAY` passed, or it ended with output while not in the panel.
    visible: bool,
    /// Output already taken, without the line being written.
    lines: VecDeque<SharedString>,
    /// The line being written; shown after `lines`.
    open: Option<SharedString>,
    /// Index in `lines` and length of the longest complete line.
    longest: (usize, usize),
    /// Old lines were dropped because of the limit.
    truncated: bool,
    scroll: UniformListScrollHandle,
    /// `None` while running.
    outcome: Option<Outcome>,
    /// Its panel was closed with `Esc` while running: it does not reopen by itself.
    dismissed: bool,
    /// Takes new output while running.
    _refresh: Option<Task<()>>,
}

/// Copying or moving entries in the background, with a row once `JOB_ROW_DELAY` passed.
struct FileTask {
    id: u64,
    /// `Copying`, `Moving`…
    verb: &'static str,
    total: usize,
    progress: Arc<fileops::Progress>,
    started: Instant,
    _work: Task<()>,
}

impl FileTask {
    fn label(&self) -> String {
        let done = self.progress.done().min(self.total);
        format!("{} {done}/{}", self.verb, self.total)
    }
}

/// How a command ended.
struct Outcome {
    status: SharedString,
    failed: bool,
}

impl Job {
    fn running(&self) -> bool {
        self.outcome.is_none()
    }

    /// It printed something visible.
    fn has_output(&self) -> bool {
        !self.lines.is_empty() || self.open.as_ref().is_some_and(|open| !open.is_empty())
    }

    fn line_count(&self) -> usize {
        self.lines.len() + usize::from(self.open.is_some())
    }

    fn line(&self, index: usize) -> Option<&SharedString> {
        match self.lines.get(index) {
            Some(line) => Some(line),
            None if index == self.lines.len() => self.open.as_ref(),
            None => None,
        }
    }

    /// Index of the longest line, counting the one being written.
    fn widest(&self) -> usize {
        let (index, length) = self.longest;
        match &self.open {
            Some(open) if open.chars().count() > length => self.lines.len(),
            _ => index,
        }
    }

    /// The panel shows the end: whatever arrives keeps showing.
    fn at_end(&self) -> bool {
        let state = self.scroll.0.borrow();
        if state.deferred_scroll_to_item.is_some() {
            return true;
        }
        let offset = state.base_handle.offset();
        let max = state.base_handle.max_offset();
        -offset.y >= max.height - px(OUTPUT_ROW_HEIGHT / 2.)
    }

    fn scroll_to_end(&self) {
        if let Some(last) = self.line_count().checked_sub(1) {
            self.scroll.scroll_to_item(last, ScrollStrategy::Top);
        }
    }

    /// Appends what is new in the output. If the panel was at the end, it follows it; if not, the
    /// view does not move even when lines are dropped from the top.
    fn apply(&mut self, delta: shell::OutputDelta) {
        let follow = self.at_end();
        let dropped = delta.dropped.min(self.lines.len());
        if dropped > 0 {
            self.lines.drain(..dropped);
            if self.longest.0 < dropped {
                self.longest = longest_line(self.lines.iter().enumerate());
            } else {
                self.longest.0 -= dropped;
            }
            if !follow {
                let base = &self.scroll.0.borrow().base_handle;
                let mut offset = base.offset();
                offset.y = (offset.y + px(OUTPUT_ROW_HEIGHT) * dropped as f32).min(px(0.));
                base.set_offset(offset);
            }
        }
        let first = self.lines.len();
        self.lines.extend(delta.lines);
        let added = longest_line(self.lines.iter().enumerate().skip(first));
        if added.1 >= self.longest.1 && added.1 > 0 {
            self.longest = added;
        }
        self.open = delta.open;
        self.truncated |= delta.truncated;
        if follow {
            self.scroll_to_end();
        }
    }
}

/// A short result in the status bar, for commands without output or that could not start.
struct Notice {
    text: SharedString,
    failed: bool,
}

pub struct Explorer {
    /// Focus of the list area (list, preview and output panel).
    focus: FocusHandle,
    mode: Mode,
    path_input: Entity<TextInput>,
    /// Why the typed path could not be opened. Cleared when editing continues.
    path_error: Option<SharedString>,
    /// Several completions for the typed path, listed under the bar.
    completion: Option<Completion>,
    /// Lookup of the typed path in progress.
    _locate: Option<Task<()>>,
    command_input: Entity<TextInput>,
    /// Why the typed command could not run. Cleared when editing continues.
    command_error: Option<SharedString>,
    history: shell::History,
    search_input: Entity<TextInput>,
    search_history: shell::History,
    /// Cursor row when the search started: where `Escape` or no match leaves it.
    search_origin: usize,
    /// Last confirmed search, for `n` and `N`.
    last_search: Option<SharedString>,
    /// The matches of `last_search` are highlighted in the list.
    search_highlight: bool,
    /// `!` commands running or ended with output to view, in the order they were started.
    jobs: Vec<Job>,
    /// The command whose output the panel shows. The others show as rows above it.
    shown: Option<u64>,
    /// The user picked `shown` with a click: another command's output does not replace it.
    pinned: bool,
    next_command: u64,
    /// Spinner step; it turns in 45° steps.
    spinner_frame: usize,
    /// A timer is advancing the spinner.
    spinning: bool,
    /// When the view was last drawn.
    last_render: Instant,
    /// Image thumbnails, and failures so they are not retried on every frame.
    previews: preview::Cache<preview::Key, Result<preview::Thumbnail, preview::Failure>>,
    /// Thumbnail being loaded. Replacing it cancels the previous one if it has not started.
    preview_pending: Option<(preview::Key, Task<()>)>,
    /// `None` if its thread could not start: then there are no image previews.
    preview_loader: Option<preview::Loader>,
    /// The quick look floats over the list with a large preview of the entry under the cursor,
    /// whatever the window width.
    quick_look: bool,
    /// Marked entries, from any folder.
    marks: Marks,
    /// Visual selection: the row where it started. The rows between it and the cursor are added
    /// to the marks with `v`.
    visual: Option<usize>,
    /// What `yy`/`xx` saved for `pp`, shared with the other windows.
    register: RegisterStore,
    /// The register as read for the last frame.
    register_view: Option<Arc<Register>>,
    /// The paths of `register_view` if it is a move, to dim them in the list.
    cut_paths: HashSet<PathBuf>,
    file_tasks: Vec<FileTask>,
    next_file_task: u64,
    /// Targets captured when opening the confirmation dialog.
    confirm_targets: Option<Vec<PathBuf>>,
    /// Whether the pending confirmation deletes permanently instead of moving to trash.
    confirm_permanent: bool,
    /// Keyboard focus of the confirmation dialog.
    confirm_focus: FocusHandle,
    notice: Option<Notice>,
    _notice_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    scroll: UniformListScrollHandle,
    home: Option<PathBuf>,
    cwd: PathBuf,
    entries: Vec<Entry>,
    error: Option<SharedString>,
    /// Row under the cursor. If the folder has a parent, row 0 is `..`.
    cursor: usize,
    /// Folder being read. Meanwhile `cwd` is still shown.
    pending: Option<PathBuf>,
    /// Read in progress. Replacing it discards the previous one.
    _load: Option<Task<()>>,
}

enum Row<'a> {
    Parent,
    Entry(&'a Entry),
}

impl Explorer {
    pub fn new(
        start: PathBuf,
        select: Option<OsString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let path_input = cx.new(TextInput::new);
        let command_input = cx.new(TextInput::new);
        let path_focus = path_input.focus_handle(cx);
        let command_focus = command_input.focus_handle(cx);
        let search_input = cx.new(TextInput::new);
        let search_focus = search_input.focus_handle(cx);
        let confirm_focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe(&path_input, |this, _, _: &text_input::Changed, cx| {
                // Typing closes the list: the next `Tab` completes the new text.
                if this.path_error.take().is_some() | this.completion.take().is_some() {
                    cx.notify();
                }
            }),
            cx.subscribe(&command_input, |this, _, _: &text_input::Changed, cx| {
                if this.command_error.take().is_some() {
                    cx.notify();
                }
            }),
            // A click on the list takes the focus and cancels editing. Switching to another
            // window also loses focus, but then the typed text is kept.
            cx.on_blur(&path_focus, window, |this, window, cx| {
                if window.is_window_active() {
                    this.leave_edit(window, cx);
                }
            }),
            cx.on_blur(&command_focus, window, |this, window, cx| {
                if window.is_window_active() {
                    this.leave_edit(window, cx);
                }
            }),
            cx.subscribe(&search_input, |this, _, _: &text_input::Changed, cx| {
                this.search_changed(cx);
            }),
            cx.on_blur(&search_focus, window, |this, window, cx| {
                if window.is_window_active() {
                    this.leave_edit(window, cx);
                }
            }),
            cx.on_blur(&confirm_focus, window, |this, window, cx| {
                if window.is_window_active() {
                    this.leave_edit(window, cx);
                }
            }),
            // Another window may have pasted here or taken entries from here, or changed the
            // register: coming back reads both again.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    let cwd = this.cwd.clone();
                    this.reload_if_in(&cwd, cx);
                }
            }),
        ];
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut explorer = Self {
            focus,
            mode: Mode::Normal,
            path_input,
            path_error: None,
            completion: None,
            _locate: None,
            command_input,
            command_error: None,
            history: shell::History::default(),
            search_input,
            search_history: shell::History::default(),
            search_origin: 0,
            last_search: None,
            search_highlight: false,
            jobs: Vec::new(),
            shown: None,
            pinned: false,
            next_command: 0,
            spinner_frame: 0,
            spinning: false,
            last_render: Instant::now(),
            previews: preview::Cache::new(PREVIEW_CACHE_SIZE),
            preview_pending: None,
            preview_loader: preview::Loader::new().ok(),
            quick_look: false,
            marks: Marks::default(),
            visual: None,
            register: RegisterStore::session(),
            register_view: None,
            cut_paths: HashSet::new(),
            file_tasks: Vec::new(),
            next_file_task: 0,
            confirm_targets: None,
            confirm_permanent: false,
            confirm_focus,
            notice: None,
            _notice_timer: None,
            _subscriptions: subscriptions,
            scroll: UniformListScrollHandle::new(),
            home,
            cwd: start.clone(),
            entries: Vec::new(),
            error: None,
            cursor: 0,
            pending: None,
            _load: None,
        };
        explorer.navigate(start, select, cx);
        explorer
    }

    /// Reads the folder in the background. Until it finishes, the previous one is still shown, so
    /// a fast folder does not flicker.
    fn navigate(&mut self, path: PathBuf, select: Option<OsString>, cx: &mut Context<Self>) {
        self.search_highlight = false;
        // The rows of a visual selection belong to the folder being left.
        self.visual = None;
        self.load(path, select, false, cx);
    }

    /// Reads the current folder again, keeping the cursor on the same entry.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(self.cwd.clone(), None, true, cx);
    }

    fn load(
        &mut self,
        path: PathBuf,
        select: Option<OsString>,
        reload: bool,
        cx: &mut Context<Self>,
    ) {
        let target = path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { listing::read_dir(&target) });
        self.pending = Some(path.clone());
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| this.show(path, select, reload, result, cx))
                .ok();
        }));
        cx.notify();
    }

    fn show(
        &mut self,
        path: PathBuf,
        select: Option<OsString>,
        reload: bool,
        result: io::Result<Vec<Entry>>,
        cx: &mut Context<Self>,
    ) {
        // When reloading, the entry under the cursor is taken now and not when the read was
        // requested: the cursor may have moved meanwhile.
        let keep = reload.then(|| (self.cursor_file_name(), self.cursor));
        self.pending = None;
        self.cwd = path;
        match result {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
                let names: HashSet<&OsStr> = self
                    .entries
                    .iter()
                    .map(|entry| entry.file_name.as_os_str())
                    .collect();
                self.marks.retain_listed(&self.cwd, &names);
            }
            Err(error) => {
                self.entries = Vec::new();
                let reason = listing::describe_error(&error);
                self.error = Some(format!("Cannot read this folder: {reason}.").into());
            }
        }
        let has_parent = self.has_parent();
        match keep {
            Some((name, previous)) => {
                self.cursor =
                    listing::reload_cursor(&self.entries, name.as_deref(), has_parent, previous);
                // Only scrolls if the cursor ended up out of view.
                self.scroll.scroll_to_item(self.cursor, ScrollStrategy::Top);
            }
            None => {
                self.cursor = listing::initial_cursor(&self.entries, select.as_deref(), has_parent);
                self.scroll
                    .scroll_to_item_strict(self.cursor, ScrollStrategy::Center);
            }
        }
        let last = self.row_count().saturating_sub(1);
        if let Some(anchor) = &mut self.visual {
            *anchor = (*anchor).min(last);
        }
        cx.notify();
    }

    /// Name of the entry under the cursor; `None` on the `..` row.
    fn cursor_file_name(&self) -> Option<OsString> {
        match self.row(self.cursor)? {
            Row::Entry(entry) => Some(entry.file_name.clone()),
            Row::Parent => None,
        }
    }

    fn has_parent(&self) -> bool {
        self.cwd.parent().is_some()
    }

    fn row_count(&self) -> usize {
        usize::from(self.has_parent()) + self.entries.len()
    }

    fn row(&self, index: usize) -> Option<Row<'_>> {
        if !self.has_parent() {
            return self.entries.get(index).map(Row::Entry);
        }
        match index {
            0 => Some(Row::Parent),
            _ => self.entries.get(index - 1).map(Row::Entry),
        }
    }

    fn set_cursor(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(last) = self.row_count().checked_sub(1) else {
            return;
        };
        let index = index.min(last);
        if index != self.cursor {
            self.cursor = index;
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let target = match self.row(self.cursor) {
            Some(Row::Parent) => None,
            Some(Row::Entry(entry)) if entry.is_dir() => Some(self.cwd.join(&entry.file_name)),
            // Opening files is not supported yet.
            _ => return,
        };
        match target {
            Some(path) => self.navigate(path, None, cx),
            None => self.go_parent(cx),
        }
    }

    /// Starts from the folder being loaded, if any: that way a quick `h h` goes up two levels even
    /// if the first read has not finished.
    fn go_parent(&mut self, cx: &mut Context<Self>) {
        let base = self.pending.as_deref().unwrap_or(&self.cwd);
        let Some(parent) = base.parent().map(Path::to_path_buf) else {
            return;
        };
        let select = base.file_name().map(OsString::from);
        self.navigate(parent, select, cx);
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.set_cursor(self.cursor.saturating_add(1), cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.set_cursor(self.cursor.saturating_sub(1), cx);
    }

    fn first(&mut self, _: &First, _: &mut Window, cx: &mut Context<Self>) {
        self.set_cursor(0, cx);
    }

    fn last(&mut self, _: &Last, _: &mut Window, cx: &mut Context<Self>) {
        self.set_cursor(usize::MAX, cx);
    }

    fn open(&mut self, _: &Open, _: &mut Window, cx: &mut Context<Self>) {
        self.open_cursor(cx);
    }

    fn parent(&mut self, _: &Parent, _: &mut Window, cx: &mut Context<Self>) {
        self.go_parent(cx);
    }

    /// Switches to insert mode with the current path in the bar and the caret at the end. It ends
    /// in `/` so the name of a subfolder can be typed right away.
    fn edit_path(&mut self, _: &EditPath, window: &mut Window, cx: &mut Context<Self>) {
        let mut text = listing::display_path(&self.cwd, self.home.as_deref());
        if !text.ends_with('/') {
            text.push('/');
        }
        self.path_input
            .update(cx, |input, cx| input.set_text(text, cx));
        self.mode = Mode::Insert;
        self.visual = None;
        self.path_error = None;
        window.focus(&self.path_input.focus_handle(cx));
        cx.notify();
    }

    /// Looks up the typed path in the background and, if it exists, goes there.
    fn confirm_path(&mut self, _: &ConfirmPath, window: &mut Window, cx: &mut Context<Self>) {
        self.completion = None;
        let text = self.path_input.read(cx).text().to_owned();
        let target = listing::resolve_input(&text, &self.cwd, self.home.as_deref());
        let locate = cx
            .background_executor()
            .spawn(async move { listing::locate(&target) });
        self._locate = Some(cx.spawn_in(window, async move |this, cx| {
            let located = locate.await;
            this.update_in(cx, |this, window, cx| match located {
                Ok((folder, select)) => {
                    this.leave_edit(window, cx);
                    this.navigate(folder, select, cx);
                }
                Err(error) => {
                    this.path_error = Some(listing::describe_path_error(&error).into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        // With the completions open, `Escape` only closes them.
        if self.completion.take().is_some() {
            cx.notify();
            return;
        }
        self.leave_edit(window, cx);
    }

    /// `Tab` / `↓` while editing the path: completes it as a shell does. The first `Tab` completes
    /// what all the matches share and, if there are several, lists them; the next ones go
    /// through the list.
    fn complete_next(&mut self, _: &CompleteNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.step_completion(true, cx) {
            self.start_completion(cx);
        }
    }

    /// `Shift+Tab` / `↑`: goes back through the completions.
    fn complete_previous(&mut self, _: &CompletePrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.step_completion(false, cx) {
            self.start_completion(cx);
        }
    }

    /// Moves through the open list and puts the candidate in the field. `false` if it is closed.
    fn step_completion(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some(completion) = &mut self.completion else {
            return false;
        };
        let count = completion.candidates.len();
        let next = match (completion.selected, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
        };
        completion.selected = Some(next);
        let text = format!("{}{}", completion.dir_text, completion.candidates[next].text());
        self.path_input.update(cx, |input, cx| input.set_text(text, cx));
        cx.notify();
        true
    }

    fn start_completion(&mut self, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_owned();
        let (dir_text, prefix) = complete::split(&text);
        let dir = listing::resolve_input(dir_text, &self.cwd, self.home.as_deref());
        let candidates = match complete::candidates(&dir, prefix) {
            Ok(candidates) => candidates,
            Err(error) => {
                self.path_error = Some(listing::describe_path_error(&error).into());
                cx.notify();
                return;
            }
        };
        if candidates.is_empty() {
            self.path_error = Some("No matches".into());
            cx.notify();
            return;
        }
        let shared = complete::common_prefix(&candidates);
        // A shared start shorter than the typed segment only differs in case: keep what was typed.
        if shared.chars().count() >= prefix.chars().count() && shared != prefix {
            let completed = format!("{dir_text}{shared}");
            self.path_input
                .update(cx, |input, cx| input.set_text(completed, cx));
        }
        self.completion = (candidates.len() > 1).then(|| Completion {
            dir_text: dir_text.to_owned(),
            candidates,
            selected: None,
        });
        cx.notify();
    }

    /// Goes back to normal mode from insert or command mode, without changing folder or running
    /// anything, and gives the focus back to the list.
    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Normal {
            return;
        }
        if self.mode == Mode::Search {
            self.set_cursor(self.search_origin, cx);
        }
        self.mode = Mode::Normal;
        self.confirm_targets = None;
        self.completion = None;
        self.path_error = None;
        self.command_error = None;
        self._locate = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn enter_command(&mut self, _: &EnterCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.enter_command_mode(CommandKind::Run, window, cx);
    }

    fn enter_launch(&mut self, _: &EnterLaunch, window: &mut Window, cx: &mut Context<Self>) {
        self.enter_command_mode(CommandKind::Launch, window, cx);
    }

    /// Switches to command mode with an empty field in the status bar.
    fn enter_command_mode(
        &mut self,
        kind: CommandKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.history.reset();
        self.command_error = None;
        self.mode = Mode::Command(kind);
        self.visual = None;
        window.focus(&self.command_input.focus_handle(cx));
        cx.notify();
    }

    /// Runs or launches the typed command in the shown folder and goes back to normal mode. An
    /// empty command only goes back.
    fn run_command(&mut self, _: &RunCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Command(kind) = self.mode else {
            return;
        };
        let text = self.command_input.read(cx).text().to_owned();
        if text.trim().is_empty() {
            self.leave_edit(window, cx);
            return;
        }
        let current = match self.row(self.cursor) {
            Some(Row::Entry(entry)) => Some(entry.file_name.clone()),
            Some(Row::Parent) => Some(OsString::from("..")),
            None => None,
        };
        let command = match shell::expand(&text, current.as_deref()) {
            Ok(command) => command,
            Err(shell::NothingSelected) => {
                self.command_error = Some("No entry under the cursor for %s".into());
                cx.notify();
                return;
            }
        };
        self.history.push(&text);
        self.leave_edit(window, cx);
        match kind {
            CommandKind::Run => self.spawn_command(command, cx),
            CommandKind::Launch => self.launch_command(command, cx),
        }
    }

    /// Starts the command and waits for it on a thread of its own: it can take as long as it
    /// wants without tying up gpui's threads. Navigating and starting others still works
    /// meanwhile.
    fn spawn_command(&mut self, command: OsString, cx: &mut Context<Self>) {
        let label: SharedString = command.to_string_lossy().into_owned().into();
        let cwd = self.cwd.clone();
        let (changed_tx, mut changed) = mpsc::unbounded::<()>();
        let spawned = shell::spawn(&command, &cwd, move || {
            changed_tx.unbounded_send(()).ok();
        });
        let process = match spawned {
            Ok(process) => process,
            Err(error) => {
                self.show_notice(format!("Could not run \"{label}\": {error}"), true, cx);
                return;
            }
        };
        let id = self.next_command;
        self.next_command += 1;
        // Output is taken at most every `LIVE_INTERVAL`: whatever arrives meanwhile piles up.
        let refresh = cx.spawn(async move |this, cx| {
            while changed.next().await.is_some() {
                let Ok(changed_lines) = this.update(cx, |this, cx| this.refresh_job(id, cx)) else {
                    break;
                };
                let interval = if changed_lines > FLOOD_LINES {
                    FLOOD_INTERVAL
                } else {
                    LIVE_INTERVAL
                };
                cx.background_executor().timer(interval).await;
            }
        });
        self.jobs.push(Job {
            id,
            command: label.clone(),
            control: process.control(),
            output: process.output(),
            started: Instant::now(),
            visible: false,
            lines: VecDeque::new(),
            open: None,
            longest: (0, 0),
            truncated: false,
            scroll: UniformListScrollHandle::new(),
            outcome: None,
            dismissed: false,
            _refresh: Some(refresh),
        });
        let wait = blocking::unblock(move || process.wait());
        cx.spawn(async move |this, cx| {
            let result = wait.await;
            this.update(cx, |this, cx| {
                this.command_finished(id, label, &cwd, result, cx)
            })
            .ok();
        })
        .detach();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(JOB_ROW_DELAY).await;
            this.update(cx, |this, cx| this.show_job_row(id, cx)).ok();
        })
        .detach();
    }

    fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|job| job.id == id)
    }

    fn job_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    fn shown_job(&self) -> Option<&Job> {
        self.shown.and_then(|id| self.job(id))
    }

    fn show_job_row(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.job_mut(id) else {
            return;
        };
        if !job.running() {
            return;
        }
        job.visible = true;
        self.start_spinner(cx);
        cx.notify();
    }

    /// Takes a command's new output. If it is the first, the command may move to the panel.
    /// Returns how many lines changed.
    fn refresh_job(&mut self, id: u64, cx: &mut Context<Self>) -> usize {
        let Some(job) = self.job_mut(id) else {
            return 0;
        };
        let had_output = job.has_output();
        let delta = job.output.take();
        let changed = delta.dropped + delta.lines.len();
        job.apply(delta);
        if !had_output && job.has_output() {
            self.output_started(id, cx);
        }
        cx.notify();
        changed
    }

    /// A command printed something for the first time. It moves to the panel if the panel is
    /// empty, shows a result that already ended, or shows an earlier command nobody picked.
    fn output_started(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.job(id) else {
            return;
        };
        if job.dismissed {
            return;
        }
        let replace = match self.shown_job() {
            None => true,
            Some(shown) => !shown.running() || !self.pinned && shown.id < id,
        };
        if replace {
            self.pinned = false;
            self.show_job(id, cx);
        }
    }

    /// Shows a command in the panel. If the panel showed a result that had ended, it is
    /// discarded: it was already seen.
    fn show_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(previous) = self.shown.filter(|previous| *previous != id) {
            self.jobs.retain(|job| job.id != previous || job.running());
        }
        self.shown = Some(id);
        if let Some(job) = self.job_mut(id) {
            job.dismissed = false;
            job.scroll_to_end();
        }
        self.start_spinner(cx);
        cx.notify();
    }

    /// Click on a row: its command moves to the panel and other commands' output does not replace it.
    fn select_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.shown != Some(id) {
            self.show_job(id, cx);
        }
        self.pinned = true;
    }

    /// Advances the spinner and the elapsed time while some running command is visible. Otherwise
    /// no timer is left.
    fn start_spinner(&mut self, cx: &mut Context<Self>) {
        if self.spinning {
            return;
        }
        self.spinning = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SPINNER_INTERVAL).await;
                let more = this.update(cx, |this, cx| {
                    let animating = !this.file_tasks.is_empty()
                        || this.jobs.iter().any(|job| {
                            job.running() && (job.visible || this.shown == Some(job.id))
                        });
                    if !animating {
                        this.spinning = false;
                        return false;
                    }
                    this.spinner_frame = this.spinner_frame.wrapping_add(1);
                    // If live output is already redrawing, the new step shows in the next frame
                    // without asking for another.
                    if this.last_render.elapsed() >= SPINNER_INTERVAL / 2 {
                        cx.notify();
                    }
                    true
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn latest_running(&self) -> Option<u64> {
        self.jobs
            .iter()
            .rev()
            .find(|job| job.running())
            .map(|job| job.id)
    }

    /// The most recent copy or move that was not asked to stop.
    fn latest_file_task(&self) -> Option<&FileTask> {
        self.file_tasks
            .iter()
            .rev()
            .find(|task| !task.progress.cancelled())
    }

    /// The most recent running command whose row or panel is in view.
    fn latest_running_in_view(&self) -> Option<u64> {
        self.jobs
            .iter()
            .rev()
            .find(|job| job.running() && (job.visible || self.shown == Some(job.id)))
            .map(|job| job.id)
    }

    /// `Ctrl+C`: stops the most recent command in view or file operation, as in a terminal. With
    /// nothing running, it copies like `yy`, as in a file manager.
    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        let job = self.latest_running_in_view();
        let job_started = job.and_then(|id| self.job(id)).map(|job| job.started);
        let task = self.latest_file_task();
        match task {
            Some(task) if job_started.is_none_or(|started| task.started > started) => {
                task.progress.cancel();
                cx.notify();
            }
            _ => match job {
                Some(id) => self.stop_job(id, cx),
                // A file operation already stopping: another press must not copy meanwhile.
                None if !self.file_tasks.is_empty() => {}
                None => self.fill_register(fileops::Op::Copy, cx),
            },
        }
    }

    fn cancel_file_task(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(task) = self.file_tasks.iter().find(|task| task.id == id) {
            task.progress.cancel();
            cx.notify();
        }
    }

    /// SIGINT the first time, SIGKILL the next.
    fn stop_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(job) = self.job(id) {
            job.control.stop();
            cx.notify();
        }
    }

    /// A row's X: stops a running command or discards an unseen result.
    fn close_job(&mut self, id: u64, cx: &mut Context<Self>) {
        match self.job(id) {
            Some(job) if job.running() => self.stop_job(id, cx),
            Some(_) => {
                self.jobs.retain(|job| job.id != id);
                cx.notify();
            }
            None => {}
        }
    }

    fn command_finished(
        &mut self,
        id: u64,
        command: SharedString,
        cwd: &Path,
        result: io::Result<shell::Exit>,
        cx: &mut Context<Self>,
    ) {
        self.reload_if_in(cwd, cx);
        cx.notify();
        let Some(job) = self.job_mut(id) else {
            return;
        };
        job._refresh = None;
        let had_output = job.has_output();
        let delta = job.output.take();
        job.apply(delta);
        let exit = match result {
            Ok(exit) => exit,
            Err(error) => {
                self.remove_job(id);
                self.show_notice(
                    format!("Could not wait for \"{command}\": {error}"),
                    true,
                    cx,
                );
                return;
            }
        };
        let (status, failed) = match exit.stopped {
            Some(shell::Stop::Interrupted) => ("interrupted".to_owned(), true),
            Some(shell::Stop::Forced) => ("killed".to_owned(), true),
            None => shell::describe_status(exit.status),
        };
        if !job.has_output() && !job.truncated {
            self.remove_job(id);
            let outcome = if failed { status } else { "done".to_owned() };
            self.show_notice(format!("! {command} · {outcome}"), failed, cx);
            return;
        }
        job.outcome = Some(Outcome {
            status: status.into(),
            failed,
        });
        if !had_output {
            self.output_started(id, cx);
        }
        if self.shown == Some(id) {
            self.notice = None;
            self._notice_timer = None;
        } else if let Some(job) = self.job_mut(id) {
            // It ended while not in the panel: its row stays until it is viewed or discarded.
            job.visible = true;
        }
    }

    fn remove_job(&mut self, id: u64) {
        self.jobs.retain(|job| job.id != id);
        if self.shown == Some(id) {
            self.shown = None;
            self.pinned = false;
        }
    }

    /// Launches the command detached and reports it in the status bar. If it ends within
    /// `LAUNCH_GRACE` with an error code, the notice says so; otherwise it says it launched.
    fn launch_command(&mut self, command: OsString, cx: &mut Context<Self>) {
        let label: SharedString = command.to_string_lossy().into_owned().into();
        let cwd = self.cwd.clone();
        let exited = match shell::launch(&command, &cwd) {
            Ok(exited) => exited,
            Err(error) => {
                self.show_notice(format!("Could not launch \"{label}\": {error}"), true, cx);
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let grace = cx.background_executor().timer(shell::LAUNCH_GRACE);
            let exited = match future::select(exited, grace).await {
                Either::Left((exited, _)) => {
                    let status = exited.ok().and_then(Result::ok);
                    this.update(cx, |this, cx| {
                        this.launch_settled(&label, status, cx);
                        this.reload_if_in(&cwd, cx);
                    })
                    .ok();
                    return;
                }
                Either::Right(((), exited)) => exited,
            };
            this.update(cx, |this, cx| this.launch_settled(&label, None, cx))
                .ok();
            // If the program ends while exporer is still open, the folder may have changed. If
            // the waiting thread was lost, there is nothing to reload.
            if exited.await.is_ok() {
                this.update(cx, |this, cx| this.reload_if_in(&cwd, cx)).ok();
            }
        })
        .detach();
    }

    /// Notice for a launched command. `status` is how it ended, if it ended within `LAUNCH_GRACE`.
    fn launch_settled(
        &mut self,
        command: &str,
        status: Option<ExitStatus>,
        cx: &mut Context<Self>,
    ) {
        match status {
            Some(status) if !status.success() => {
                let (described, _) = shell::describe_status(status);
                let text = match shell::exit_reason(status) {
                    Some(reason) => format!("& {command} · {described} · {reason}"),
                    None => format!("& {command} · {described}"),
                };
                self.show_notice(text, true, cx);
            }
            _ => self.show_notice(format!("& {command} · launched"), false, cx),
        }
    }

    /// Reads the folder again if still in `cwd`: a command may have changed it. If another folder
    /// is shown, there is no need.
    fn reload_if_in(&mut self, cwd: &Path, cx: &mut Context<Self>) {
        let elsewhere = self
            .pending
            .as_ref()
            .is_some_and(|pending| *pending != self.cwd);
        if self.cwd == cwd && !elsewhere {
            self.reload(cx);
        }
    }

    /// Full path of the entry in `row`; `None` for `..`.
    fn row_path(&self, row: usize) -> Option<PathBuf> {
        match self.row(row)? {
            Row::Entry(entry) => Some(self.cwd.join(&entry.file_name)),
            Row::Parent => None,
        }
    }

    fn row_paths(&self, rows: impl Iterator<Item = usize>) -> Vec<PathBuf> {
        rows.filter_map(|row| self.row_path(row)).collect()
    }

    fn is_marked(&self, row: usize) -> bool {
        let in_visual = self
            .visual
            .is_some_and(|anchor| selection::visual_rows(anchor, self.cursor).contains(&row));
        if in_visual {
            return matches!(self.row(row), Some(Row::Entry(_)));
        }
        !self.marks.is_empty() && self.row_path(row).is_some_and(|path| self.marks.contains(&path))
    }

    /// The entry in `row` was cut and has not been pasted yet.
    fn is_cut(&self, row: usize) -> bool {
        !self.cut_paths.is_empty()
            && self
                .row_path(row)
                .is_some_and(|path| self.cut_paths.contains(&path))
    }

    /// What an operation acts on: the marks plus a visual selection, or the entry under the
    /// cursor. Ends the visual selection.
    fn take_targets(&mut self) -> Vec<PathBuf> {
        if let Some(anchor) = self.visual.take() {
            let rows = selection::visual_rows(anchor, self.cursor);
            self.marks.extend(self.row_paths(rows));
        }
        selection::targets(&self.marks, self.row_path(self.cursor))
    }

    /// `Tab`: marks or unmarks the entry under the cursor and moves down.
    fn toggle_mark(&mut self, _: &ToggleMark, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.row_path(self.cursor) {
            self.marks.toggle(path);
            cx.notify();
        }
        self.set_cursor(self.cursor.saturating_add(1), cx);
    }

    /// `v`: starts a visual selection on the cursor row, or adds it to the marks and ends it.
    fn toggle_visual(&mut self, _: &ToggleVisual, _: &mut Window, cx: &mut Context<Self>) {
        match self.visual.take() {
            Some(anchor) => {
                let rows = selection::visual_rows(anchor, self.cursor);
                self.marks.extend(self.row_paths(rows));
            }
            None => self.visual = Some(self.cursor),
        }
        cx.notify();
    }

    /// `Shift+↓`: starts a visual selection on the cursor row if there is none and moves down.
    fn extend_down(&mut self, _: &ExtendDown, _: &mut Window, cx: &mut Context<Self>) {
        self.visual.get_or_insert(self.cursor);
        self.set_cursor(self.cursor.saturating_add(1), cx);
        cx.notify();
    }

    /// `Shift+↑`: like `Shift+↓`, moving up.
    fn extend_up(&mut self, _: &ExtendUp, _: &mut Window, cx: &mut Context<Self>) {
        self.visual.get_or_insert(self.cursor);
        self.set_cursor(self.cursor.saturating_sub(1), cx);
        cx.notify();
    }

    /// `Ctrl+A`: marks every entry of the folder.
    fn mark_all(&mut self, _: &MarkAll, _: &mut Window, cx: &mut Context<Self>) {
        self.marks.extend(
            self.entries
                .iter()
                .map(|entry| self.cwd.join(&entry.file_name)),
        );
        cx.notify();
    }

    /// `yy`: copies the targets into the register.
    fn yank(&mut self, _: &Yank, _: &mut Window, cx: &mut Context<Self>) {
        self.fill_register(fileops::Op::Copy, cx);
    }

    /// `xx`: cuts the targets into the register; `pp` moves them.
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        self.fill_register(fileops::Op::Move, cx);
    }

    fn fill_register(&mut self, op: fileops::Op, cx: &mut Context<Self>) {
        let paths = self.take_targets();
        if paths.is_empty() {
            cx.notify();
            return;
        }
        let count = paths.len();
        let register = Register { op, paths };
        match self.register.save(Some(&register)) {
            Ok(()) => {
                self.marks.clear();
                let done = match op {
                    fileops::Op::Copy => "copied",
                    fileops::Op::Move => "cut",
                };
                self.show_notice(format!("{} {done}", items(count)), false, cx);
            }
            Err(error) => {
                let reason = fileops::describe(&error);
                self.show_notice(format!("Could not save the register: {reason}"), true, cx);
            }
        }
    }

    /// `pp`: copies or moves the register's entries into the shown folder, in the background.
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(register) = self.register.load().map(Arc::unwrap_or_clone) else {
            self.show_notice(
                "Nothing to paste: copy with yy or cut with xx".into(),
                true,
                cx,
            );
            return;
        };
        let dir = self.cwd.clone();
        let verb = match register.op {
            fileops::Op::Copy => "Copying",
            fileops::Op::Move => "Moving",
        };
        let work = {
            let register = register.clone();
            let dir = dir.clone();
            move |progress: &fileops::Progress| {
                fileops::paste(register.op, &register.paths, &dir, progress)
            }
        };
        self.start_file_task(
            verb,
            register.paths.len(),
            work,
            cx,
            move |this, report, cx| {
                this.paste_finished(register, dir, report, cx);
            },
        );
    }

    /// Runs `work` on a thread of its own with a row that shows its progress, then `done` with
    /// its report.
    fn start_file_task(
        &mut self,
        verb: &'static str,
        total: usize,
        work: impl FnOnce(&fileops::Progress) -> fileops::Report + Send + 'static,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, fileops::Report, &mut Context<Self>) + 'static,
    ) {
        let progress = Arc::new(fileops::Progress::default());
        let id = self.next_file_task;
        self.next_file_task += 1;
        let running = {
            let progress = progress.clone();
            blocking::unblock(move || work(&progress))
        };
        let watcher = cx.spawn(async move |this, cx| {
            let report = running.await;
            this.update(cx, |this, cx| {
                this.file_tasks.retain(|task| task.id != id);
                done(this, report, cx);
            })
            .ok();
        });
        self.file_tasks.push(FileTask {
            id,
            verb,
            total,
            progress,
            started: Instant::now(),
            _work: watcher,
        });
        self.start_spinner(cx);
        cx.notify();
    }

    fn paste_finished(
        &mut self,
        register: Register,
        dir: PathBuf,
        report: fileops::Report,
        cx: &mut Context<Self>,
    ) {
        // A move empties the register, except for what could not be moved. If it was replaced
        // meanwhile (another `yy` or `xx`), it is left alone.
        if register.op == fileops::Op::Move && self.register.load().as_deref() == Some(&register) {
            let left: Vec<PathBuf> = report
                .failures
                .iter()
                .map(|failure| failure.path.clone())
                .chain(report.skipped.iter().cloned())
                .collect();
            let left = (!left.is_empty()).then_some(Register {
                op: register.op,
                paths: left,
            });
            self.register.save(left.as_ref()).ok();
        }
        let verb = match register.op {
            fileops::Op::Copy => "Pasted",
            fileops::Op::Move => "Moved",
        };
        let (text, failed) = report_text(verb, "", register.paths.len(), &report);
        self.show_notice(text, failed, cx);

        let elsewhere = self
            .pending
            .as_ref()
            .is_some_and(|pending| *pending != self.cwd);
        if self.cwd == dir && !elsewhere {
            match report.done.first().and_then(|path| path.file_name()) {
                Some(name) => {
                    let name = name.to_owned();
                    self.load(dir, Some(name), false, cx);
                }
                None => self.reload(cx),
            }
        } else {
            // A move may have taken entries from the shown folder.
            let cwd = self.cwd.clone();
            self.reload_if_in(&cwd, cx);
        }
    }

    /// `dd`: asks before moving the targets to the trash.
    fn trash(&mut self, _: &TrashEntries, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_removal(false, window, cx);
    }

    fn move_to_trash(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.marks.clear();
        let total = paths.len();
        let work = move |progress: &fileops::Progress| {
            fileops::trash_all(fileops::Trash::session().as_ref(), &paths, progress)
        };
        self.start_file_task(
            "Moving to the trash",
            total,
            work,
            cx,
            move |this, report, cx| {
                let (mut text, failed) = report_text("Moved", " to the trash", total, &report);
                if failed {
                    text.push_str(" · D deletes permanently");
                }
                this.show_notice(text, failed, cx);
                this.after_removal(cx);
            },
        );
    }

    /// `D`: asks before deleting the targets for good.
    fn ask_delete(&mut self, _: &DeleteEntries, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_removal(true, window, cx);
    }

    fn ask_removal(&mut self, permanent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.take_targets();
        if paths.is_empty() {
            cx.notify();
            return;
        }
        self.confirm_targets = Some(paths);
        self.confirm_permanent = permanent;
        self.mode = Mode::Confirm;
        window.focus(&self.confirm_focus);
        cx.notify();
    }

    /// Confirm the captured targets, using the removal method shown in the dialog.
    fn confirm_delete(&mut self, _: &ConfirmDelete, window: &mut Window, cx: &mut Context<Self>) {
        let Some(paths) = self.confirm_targets.take() else {
            return;
        };
        let permanent = self.confirm_permanent;
        self.leave_edit(window, cx);
        if !permanent {
            self.move_to_trash(paths, cx);
            return;
        }
        self.marks.clear();
        let total = paths.len();
        let work = move |progress: &fileops::Progress| fileops::delete_all(&paths, progress);
        self.start_file_task("Deleting", total, work, cx, move |this, report, cx| {
            let (text, failed) = report_text("Deleted", "", total, &report);
            this.show_notice(text, failed, cx);
            this.after_removal(cx);
        });
    }

    /// After deleting, the shown folder is read again; the cursor stays on the same row, which
    /// now holds the entry after the last one removed.
    fn after_removal(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        self.reload_if_in(&cwd, cx);
    }

    /// `Space`: opens or closes the quick look.
    fn toggle_quick_look(&mut self, _: &ToggleQuickLook, _: &mut Window, cx: &mut Context<Self>) {
        self.quick_look = !self.quick_look;
        cx.notify();
    }

    /// The thumbnail of the entry under the cursor, or why there is none, once it is loaded.
    fn cursor_thumbnail(&self) -> Option<&Result<preview::Thumbnail, preview::Failure>> {
        self.cursor_preview_key()
            .and_then(|key| self.previews.peek(&key))
    }

    /// Key of the image preview for the entry under the cursor, if it can have one.
    fn cursor_preview_key(&self) -> Option<preview::Key> {
        match self.row(self.cursor)? {
            Row::Entry(entry) if !entry.is_dir() && preview::can_preview(&entry.name) => {
                Some(preview::Key {
                    path: self.cwd.join(&entry.file_name),
                    len: entry.len,
                    modified: entry.modified,
                })
            }
            _ => None,
        }
    }

    /// Starts loading the thumbnail of the entry under the cursor, unless it is cached or
    /// already loading. Called while drawing, so every way of moving the cursor is covered.
    fn request_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(key), Some(loader)) = (self.cursor_preview_key(), self.preview_loader.clone())
        else {
            self.preview_pending = None;
            return;
        };
        if self.previews.touch(&key) {
            return;
        }
        if self
            .preview_pending
            .as_ref()
            .is_some_and(|(pending, _)| *pending == key)
        {
            return;
        }
        let path = key.path.clone();
        let loaded_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DELAY).await;
            let result = loader
                .load(path)
                .await
                .unwrap_or(Err(preview::Failure::Unreadable));
            this.update_in(cx, |this, window, cx| {
                this.preview_pending = None;
                // Free the GPU texture of the thumbnail that leaves the cache.
                if let Some(Ok(evicted)) = this.previews.insert(loaded_key, result) {
                    cx.drop_image(evicted.image, Some(window));
                }
                cx.notify();
            })
            .ok();
        });
        self.preview_pending = Some((key, task));
    }

    fn show_notice(&mut self, text: String, failed: bool, cx: &mut Context<Self>) {
        self.notice = Some(Notice {
            text: text.into(),
            failed,
        });
        self._notice_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_DURATION).await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Switches to search mode with an empty field in the status bar.
    fn enter_search(&mut self, _: &EnterSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.search_history.reset();
        self.search_origin = self.cursor;
        self.mode = Mode::Search;
        window.focus(&self.search_input.focus_handle(cx));
        cx.notify();
    }

    /// Incremental search: the cursor goes to the first match from the row where the search
    /// started, or back to that row if nothing matches.
    fn search_changed(&mut self, cx: &mut Context<Self>) {
        if self.mode != Mode::Search {
            return;
        }
        let pattern = self.search_input.read(cx).text().to_owned();
        let row = self
            .find_match(&pattern, self.search_origin, Direction::Forward, true)
            .map_or(self.search_origin, |(row, _)| row);
        self.set_cursor(row, cx);
        cx.notify();
    }

    /// `Enter` while searching: keeps the cursor on the match and remembers the search for `n`
    /// and `N`.
    fn confirm_search(&mut self, _: &ConfirmSearch, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search_input.read(cx).text().to_owned();
        if text.is_empty() {
            self.leave_edit(window, cx);
            return;
        }
        self.search_history.push(&text);
        let found = self.match_count(&text) > 0;
        if !found {
            self.show_notice(format!("/{text} · no match"), true, cx);
        }
        self.last_search = Some(text.into());
        self.search_highlight = true;
        // Normal mode first, so losing the field's focus does not cancel the search.
        self.mode = Mode::Normal;
        window.focus(&self.focus);
        cx.notify();
    }

    fn next_match(&mut self, _: &NextMatch, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_match(Direction::Forward, cx);
    }

    fn previous_match(&mut self, _: &PreviousMatch, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_match(Direction::Backward, cx);
    }

    /// `n`/`N`: the next or previous match of the last search, wrapping around the ends.
    fn jump_to_match(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(pattern) = self.last_search.clone() else {
            return;
        };
        self.search_highlight = true;
        match self.find_match(&pattern, self.cursor, direction, false) {
            Some((row, wrapped)) => {
                self.set_cursor(row, cx);
                let total = self.match_count(&pattern);
                let position = (0..=row)
                    .filter(|&row| self.row_matches(row, &pattern))
                    .count();
                let mut text = format!("/{pattern} · {position}/{total}");
                if wrapped {
                    text.push_str(" · wrapped");
                }
                self.show_notice(text, false, cx);
            }
            None => self.show_notice(format!("/{pattern} · no match"), true, cx),
        }
        cx.notify();
    }

    fn row_matches(&self, row: usize, pattern: &str) -> bool {
        match self.row(row) {
            Some(Row::Entry(entry)) => search::matches(&entry.name, pattern),
            _ => false,
        }
    }

    fn match_count(&self, pattern: &str) -> usize {
        (0..self.row_count())
            .filter(|&row| self.row_matches(row, pattern))
            .count()
    }

    fn find_match(
        &self,
        pattern: &str,
        from: usize,
        direction: Direction,
        include_from: bool,
    ) -> Option<(usize, bool)> {
        if pattern.is_empty() {
            return None;
        }
        search::next_match(self.row_count(), from, direction, include_from, |row| {
            self.row_matches(row, pattern)
        })
    }

    /// The pattern whose matches are highlighted in the list: what is being typed while
    /// searching, or the last search while its highlight is on.
    fn highlighted_pattern(&self, cx: &App) -> Option<SharedString> {
        match self.mode {
            Mode::Search => {
                let text = self.search_input.read(cx).text();
                (!text.is_empty()).then(|| SharedString::from(text.to_owned()))
            }
            _ if self.search_highlight => self.last_search.clone(),
            _ => None,
        }
    }

    fn search_history_older(&mut self, _: &HistoryOlder, _: &mut Window, cx: &mut Context<Self>) {
        let current = self.search_input.read(cx).text().to_owned();
        let older = self.search_history.older(&current).map(str::to_owned);
        self.show_in_search_line(older, cx);
    }

    fn search_history_newer(&mut self, _: &HistoryNewer, _: &mut Window, cx: &mut Context<Self>) {
        let newer = self.search_history.newer().map(str::to_owned);
        self.show_in_search_line(newer, cx);
    }

    fn show_in_search_line(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        let Some(text) = text else {
            return;
        };
        self.search_input
            .update(cx, |input, cx| input.set_text(text, cx));
        // `set_text` does not emit `Changed`.
        self.search_changed(cx);
    }

    fn history_older(&mut self, _: &HistoryOlder, _: &mut Window, cx: &mut Context<Self>) {
        let current = self.command_input.read(cx).text().to_owned();
        let older = self.history.older(&current).map(str::to_owned);
        self.show_in_command_line(older, cx);
    }

    fn history_newer(&mut self, _: &HistoryNewer, _: &mut Window, cx: &mut Context<Self>) {
        let newer = self.history.newer().map(str::to_owned);
        self.show_in_command_line(newer, cx);
    }

    fn show_in_command_line(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        let Some(text) = text else {
            return;
        };
        self.command_input
            .update(cx, |input, cx| input.set_text(text, cx));
        self.command_error = None;
        cx.notify();
    }

    /// `Esc`: closes the quick look, or else the panel. A result that ended is discarded; a
    /// running command stays as a row and its output does not reopen the panel by itself.
    /// Without a panel, it clears the search highlight.
    fn close_output(&mut self, _: &CloseOutput, _: &mut Window, cx: &mut Context<Self>) {
        if self.visual.take().is_some() {
            cx.notify();
            return;
        }
        if self.quick_look {
            self.quick_look = false;
            cx.notify();
            return;
        }
        let Some(id) = self.shown.take() else {
            if self.search_highlight {
                self.search_highlight = false;
                cx.notify();
            } else if !self.marks.is_empty() {
                self.marks.clear();
                cx.notify();
            } else {
                cx.propagate();
            }
            return;
        };
        self.pinned = false;
        match self.job_mut(id) {
            Some(job) if job.running() => job.dismissed = true,
            _ => self.jobs.retain(|job| job.id != id),
        }
        cx.notify();
    }

    fn click_row(&mut self, index: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            let rows = selection::visual_rows(self.cursor, index);
            self.marks.extend(self.row_paths(rows));
            self.set_cursor(index, cx);
            cx.notify();
            return;
        }
        if modifiers.control {
            if let Some(path) = self.row_path(index) {
                self.marks.toggle(path);
            }
            self.set_cursor(index, cx);
            cx.notify();
            return;
        }
        self.set_cursor(index, cx);
        if event.click_count() >= 2 {
            self.open_cursor(cx);
        }
    }

    fn render_header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let bar = div()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .px(px(14.))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(t.border)
            .font_family(MONO_FONT)
            .text_size(px(16.))
            .font_weight(FontWeight::MEDIUM);

        if self.mode == Mode::Insert {
            let error = self
                .path_error
                .clone()
                .map(|error| alert(t, error).ml(px(12.)));
            return bar
                .key_context(PATH_BAR_CONTEXT)
                .on_action(cx.listener(Self::confirm_path))
                .on_action(cx.listener(Self::cancel_edit))
                .on_action(cx.listener(Self::complete_next))
                .on_action(cx.listener(Self::complete_previous))
                .text_color(t.text_bright)
                .child(self.path_input.clone())
                .children(error);
        }

        let display = listing::display_path(&self.cwd, self.home.as_deref());
        // The bar's padding and a safety margin.
        let available = f32::from(window.viewport_size().width) - 48.;
        let budget = (available / PATH_CHAR_WIDTH).max(8.) as usize;
        let (rooted, segments) = breadcrumb(&display, budget);
        let last = segments.len().saturating_sub(1);

        let separator = || div().px(px(6.)).text_color(t.path_separator).child("/");
        let mut crumbs: Vec<Div> = Vec::with_capacity(segments.len() * 2 + 1);
        if rooted {
            crumbs.push(separator());
        }
        for (index, segment) in segments.into_iter().enumerate() {
            if index > 0 {
                crumbs.push(separator());
            }
            let crumb = div().child(SharedString::from(segment.to_owned()));
            crumbs.push(if index == last {
                crumb
                    .text_color(t.text_bright)
                    .font_weight(FontWeight::BOLD)
            } else {
                crumb.text_color(t.path_segment)
            });
        }

        bar.child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .children(crumbs),
        )
    }

    fn render_list(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        // 53 px = list padding (8) + border (1) + row padding (10) + mark column (22)
        // + gap (12).
        let header = div()
            .flex_none()
            .h(px(SECTION_HEADER_HEIGHT))
            .pl(px(53.))
            .pr(px(19.))
            .flex()
            .items_center()
            .gap(px(12.))
            .border_b_1()
            .border_color(t.border)
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text_faint)
            .child(div().flex_1().child("NAME"))
            .child(div().flex_none().w(px(88.)).text_right().child("SIZE"))
            .child(div().flex_none().w(px(140.)).child("MODIFIED"))
            .child(div().flex_none().w(px(96.)).child("PERMISSIONS"));

        let body = if self.error.is_some() || self.entries.is_empty() {
            let (icon_path, color, message): (&'static str, Hsla, SharedString) = match &self.error
            {
                Some(error) => (icon::ALERT, t.caret, error.clone()),
                None => (icon::FOLDER, t.text_faint, "Empty folder".into()),
            };
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .p(px(8.))
                .when(self.has_parent(), |body| {
                    body.child(self.render_row(0, t, None, cx))
                })
                .child(
                    div()
                        .mt(px(8.))
                        .px(px(12.))
                        .py(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(t.border)
                        .text_color(t.text_muted)
                        .child(icon(icon_path, 16., color))
                        .child(div().flex_1().child(message)),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .p(px(8.))
                .child(
                    uniform_list(
                        "entries",
                        self.row_count(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            let t = Theme::get(cx);
                            let pattern = this.highlighted_pattern(cx);
                            range
                                .map(|index| {
                                    this.render_row(
                                        index,
                                        &t,
                                        pattern.as_ref().map(AsRef::as_ref),
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(self.scroll.clone())
                    .size_full(),
                )
                .into_any_element()
        };

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(body)
    }

    fn render_row(
        &self,
        index: usize,
        t: &Theme,
        pattern: Option<&str>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let is_cursor = index == self.cursor;
        let marked = self.is_marked(index);
        let cut = self.is_cut(index);
        let row = div()
            .id(index)
            // Without full width, each uniform_list row takes the width of its content and the
            // columns do not line up.
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(12.))
            .rounded(px(8.))
            .border_1()
            .font_family(MONO_FONT)
            .text_size(px(12.5))
            .whitespace_nowrap()
            .map(|row| {
                if is_cursor {
                    row.bg(t.cursor_bg).border_color(t.accent)
                } else if marked {
                    row.bg(t.mark_bg).border_color(transparent_black())
                } else {
                    row.border_color(transparent_black())
                        .hover(|style| style.bg(t.raised_bg))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                this.click_row(index, event, cx);
            }))
            .when(cut, |row| row.opacity(0.45))
            .child(
                div()
                    .flex_none()
                    .w(px(22.))
                    .flex()
                    .justify_center()
                    .when(marked, |column| {
                        column.child(icon(icon::CHECK, 14., t.accent))
                    }),
            );

        match self.row(index) {
            Some(Row::Entry(entry)) => {
                let (icon_color, weight, size_color) = if entry.is_dir() {
                    (t.accent, FontWeight::MEDIUM, t.text_faint)
                } else {
                    (t.text_muted, FontWeight::NORMAL, t.text_muted)
                };
                row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .text_color(t.text)
                        .font_weight(weight)
                        .child(icon(entry.kind.icon(), 16., icon_color))
                        .child(div().min_w_0().truncate().child(highlighted_name(
                            t,
                            entry.name.clone(),
                            pattern,
                        ))),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(88.))
                        .text_right()
                        .text_color(size_color)
                        .child(entry.size_text.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(140.))
                        .text_color(t.text_muted)
                        .child(entry.modified_text.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(96.))
                        .text_color(t.text_faint)
                        .child(entry.permissions_text.clone()),
                )
            }
            Some(Row::Parent) | None => row
                .text_color(t.text_faint)
                .when(!is_cursor, |row| row.opacity(0.6))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(icon(icon::PARENT, 16., t.text_faint))
                        .child(".."),
                ),
        }
    }

    /// The list of completions, floating under the path bar over the rest of the window.
    fn render_completion(&self, t: &Theme) -> Option<Div> {
        let completion = self.completion.as_ref()?;
        let count = completion.candidates.len();
        // A window of rows that keeps the selected one in view.
        let first = completion
            .selected
            .map_or(0, |index| (index + 1).saturating_sub(COMPLETION_ROWS))
            .min(count.saturating_sub(COMPLETION_ROWS));
        let rows = completion
            .candidates
            .iter()
            .enumerate()
            .skip(first)
            .take(COMPLETION_ROWS)
            .map(|(index, candidate)| {
                let selected = completion.selected == Some(index);
                let (icon_path, color) = if candidate.is_dir {
                    (icon::FOLDER, t.accent)
                } else {
                    (
                        listing::kind_of(&candidate.name, false).icon(),
                        t.text_muted,
                    )
                };
                div()
                    .h(px(COMPLETION_ROW_HEIGHT))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(6.))
                    .border_1()
                    .whitespace_nowrap()
                    .map(|row| {
                        if selected {
                            row.bg(t.cursor_bg).border_color(t.accent)
                        } else {
                            row.border_color(transparent_black())
                        }
                    })
                    .child(icon(icon_path, 14., color))
                    .child(div().min_w_0().truncate().child(candidate.text()))
            });
        let above = (first > 0).then(|| format!("{first} more above"));
        let below = count - first - count.min(COMPLETION_ROWS).min(count - first);
        let below = (below > 0).then(|| format!("{below} more"));
        let more = |text: String| {
            div()
                .h(px(20.))
                .px(px(10.))
                .flex()
                .items_center()
                .text_size(px(11.))
                .text_color(t.text_faint)
                .font_family(UI_FONT)
                .child(text)
        };
        Some(
            div()
                .absolute()
                .top(px(HEADER_HEIGHT + 2.))
                .left(px(8.))
                .min_w(px(280.))
                .max_w(px(640.))
                .p(px(4.))
                .flex()
                .flex_col()
                .rounded(px(10.))
                .border_1()
                .border_color(t.border_strong)
                .bg(t.raised_bg)
                .shadow_lg()
                .font_family(MONO_FONT)
                .text_size(px(13.))
                .text_color(t.text)
                .children(above.map(more))
                .children(rows)
                .children(below.map(more)),
        )
    }

    fn render_preview(&self, t: &Theme) -> impl IntoElement {
        let header = div()
            .flex_none()
            .h(px(SECTION_HEADER_HEIGHT))
            .px(px(16.))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(t.border)
            .child(section_title(t, "PREVIEW"));

        let frame = |height: f32| {
            div()
                .h(px(height))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.))
                .border_1()
                .border_color(t.border_strong)
                .overflow_hidden()
        };
        let icon_frame = |icon_path: &'static str, message: Option<&'static str>| {
            frame(190.).child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(icon(icon_path, 48., t.text_faint))
                    .children(message.map(|message| {
                        div()
                            .text_size(px(11.))
                            .text_color(t.text_faint)
                            .child(message)
                    })),
            )
        };
        let title = |name: SharedString, subtitle: SharedString| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_family(MONO_FONT)
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text)
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.text_faint)
                        .child(subtitle),
                )
        };

        let content = match self.row(self.cursor) {
            Some(Row::Entry(entry)) => {
                let thumbnail = self.cursor_thumbnail();
                let subtitle = entry_subtitle(entry, thumbnail);
                let image_frame = match thumbnail {
                    Some(Ok(thumbnail)) => {
                        let proportion = thumbnail.height as f32 / thumbnail.width.max(1) as f32;
                        let height = (PREVIEW_IMAGE_WIDTH * proportion + 16.)
                            .clamp(PREVIEW_FRAME_MIN, PREVIEW_FRAME_MAX);
                        frame(height).p(px(8.)).child(
                            img(ImageSource::Render(thumbnail.image.clone()))
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                    }
                    Some(Err(preview::Failure::TooLarge)) => {
                        icon_frame(entry.kind.icon(), Some("Too large to preview"))
                    }
                    Some(Err(preview::Failure::Unreadable)) => {
                        icon_frame(entry.kind.icon(), Some("No preview"))
                    }
                    // Not an image, or still loading.
                    None => icon_frame(entry.kind.icon(), None),
                };
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(image_frame)
                        .child(title(entry.name.clone(), subtitle))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.))
                                        .child(card(
                                            t,
                                            "Modified",
                                            entry.modified_text.clone(),
                                            false,
                                        ))
                                        .child(card(t, "Size", entry.size_text.clone(), false)),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.))
                                        .child(card(
                                            t,
                                            "Permissions",
                                            entry.permissions_text.clone(),
                                            true,
                                        ))
                                        .child(card(t, "Type", entry.type_text.clone(), false)),
                                ),
                        ),
                )
            }
            Some(Row::Parent) => {
                let parent = self.cwd.parent().unwrap_or(&self.cwd);
                let subtitle = listing::display_path(parent, self.home.as_deref());
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(icon_frame(icon::PARENT, None))
                        .child(title("..".into(), subtitle.into())),
                )
            }
            None => None,
        };

        div()
            .flex_none()
            .w(px(360.))
            .border_l_1()
            .border_color(t.border)
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(div().p(px(14.)).children(content))
    }

    /// The quick look: a large preview of the entry under the cursor floating over the list.
    /// `None` when closed or when there is nothing under the cursor.
    fn render_quick_look(&self, t: &Theme) -> Option<Div> {
        if !self.quick_look {
            return None;
        }
        let large_icon = |icon_path: &'static str, message: Option<&'static str>| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.))
                .child(icon(icon_path, 72., t.text_faint))
                .children(message.map(|message| {
                    div()
                        .text_size(px(12.))
                        .text_color(t.text_faint)
                        .child(message)
                }))
                .into_any_element()
        };
        let (name, subtitle, body): (SharedString, SharedString, AnyElement) =
            match self.row(self.cursor)? {
                Row::Entry(entry) => {
                    let thumbnail = self.cursor_thumbnail();
                    let subtitle = if entry.is_dir() {
                        format!("{} · {}", entry.type_text, entry.modified_text)
                    } else {
                        format!(
                            "{} · {}",
                            entry_subtitle(entry, thumbnail),
                            entry.modified_text
                        )
                    };
                    let body = match thumbnail {
                        // Never enlarged past the thumbnail: small images stay sharp.
                        Some(Ok(thumbnail)) => img(ImageSource::Render(thumbnail.image.clone()))
                            .size_full()
                            .object_fit(ObjectFit::ScaleDown)
                            .into_any_element(),
                        Some(Err(preview::Failure::TooLarge)) => {
                            large_icon(entry.kind.icon(), Some("Too large to preview"))
                        }
                        Some(Err(preview::Failure::Unreadable)) => {
                            large_icon(entry.kind.icon(), Some("No preview"))
                        }
                        None => large_icon(entry.kind.icon(), None),
                    };
                    (entry.name.clone(), subtitle.into(), body)
                }
                Row::Parent => {
                    let parent = self.cwd.parent().unwrap_or(&self.cwd);
                    let subtitle = listing::display_path(parent, self.home.as_deref());
                    ("..".into(), subtitle.into(), large_icon(icon::PARENT, None))
                }
            };
        let card = div()
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .border_1()
            .border_color(t.border_strong)
            .bg(t.bg)
            .child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .font_family(MONO_FONT)
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.text)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(t.text_faint)
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        hint(t, &["Space"], "close")
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(t.text_faint),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(body),
            );
        // An opaque backdrop over the whole list area, so nothing of the list shows around the
        // card. Clicks do not go through to the list.
        Some(
            div()
                .absolute()
                .inset_0()
                .p(px(12.))
                .bg(t.bg)
                .occlude()
                .child(card),
        )
    }

    /// Panel with a command's output, full width. It takes what it needs up to
    /// `OUTPUT_MAX_FRACTION` of the window height; the rest scrolls with the wheel.
    fn render_output(
        &self,
        job: &Job,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let padding_y = 6.;
        let content = OUTPUT_HEADER_HEIGHT
            + job.line_count() as f32 * OUTPUT_ROW_HEIGHT
            + 2. * padding_y
            + 1.;
        let max = f32::from(window.viewport_size().height) * OUTPUT_MAX_FRACTION;
        let height = content.min(max).max(OUTPUT_HEADER_HEIGHT);

        let header = div()
            .flex_none()
            .h(px(OUTPUT_HEADER_HEIGHT))
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.border)
            .whitespace_nowrap()
            .text_size(px(11.))
            .text_color(t.text_faint)
            .when(job.running(), |header| header.child(self.spinner(t)))
            .child(command_label(t, job.command.clone()))
            .when(job.truncated, |header| {
                header.child(
                    div()
                        .flex_none()
                        .text_color(t.caret)
                        .child("truncated to the last MiB"),
                )
            })
            .map(|header| match &job.outcome {
                None => header.children(self.render_job_state(job, t, cx)),
                Some(outcome) => header.child(
                    div()
                        .flex_none()
                        .font_family(MONO_FONT)
                        .text_size(px(11.5))
                        .text_color(if outcome.failed {
                            t.caret
                        } else {
                            t.text_faint
                        })
                        .child(outcome.status.clone()),
                ),
            })
            .child(hint(t, &["Esc"], "close").flex_none().pr(px(6.)));

        let list = uniform_list(
            "command-output",
            job.line_count(),
            cx.processor(|this, range: Range<usize>, _window, _cx| {
                let Some(job) = this.shown_job() else {
                    return Vec::new();
                };
                range
                    .filter_map(|index| job.line(index))
                    .map(|line| div().h(px(OUTPUT_ROW_HEIGHT)).child(line.clone()))
                    .collect::<Vec<_>>()
            }),
        )
        // Long lines are not wrapped: the panel scrolls horizontally.
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(job.widest()))
        .track_scroll(job.scroll.clone())
        .size_full();

        div()
            .flex_none()
            .h(px(height))
            .border_t_1()
            .border_color(t.border)
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(14.))
                    .py(px(padding_y))
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .line_height(px(OUTPUT_ROW_HEIGHT))
                    .whitespace_nowrap()
                    .text_color(t.text_muted)
                    .child(list),
            )
    }

    /// Rows of the commands not in the panel, in the order they were started, above it. `None` if
    /// none is visible.
    fn render_jobs(&self, t: &Theme, cx: &Context<Self>) -> Option<Div> {
        let visible: Vec<&Job> = self
            .jobs
            .iter()
            .filter(|job| job.visible && self.shown != Some(job.id))
            .collect();
        let tasks: Vec<&FileTask> = self
            .file_tasks
            .iter()
            .filter(|task| task.started.elapsed() >= JOB_ROW_DELAY)
            .collect();
        if visible.is_empty() && tasks.is_empty() {
            return None;
        }
        let hidden = visible.len().saturating_sub(MAX_JOB_ROWS);
        let more = (hidden > 0).then(|| {
            div()
                .h(px(24.))
                .px(px(14.))
                .flex()
                .items_center()
                .text_size(px(11.))
                .text_color(t.text_faint)
                .child(format!("+{hidden} more"))
        });
        let rows = visible[hidden..]
            .iter()
            .map(|job| self.render_job(job, t, cx));
        Some(
            div()
                .flex_none()
                .border_t_1()
                .border_color(t.border)
                .flex()
                .flex_col()
                .children(more)
                .children(rows)
                .children(
                    tasks
                        .into_iter()
                        .map(|task| self.render_file_task(task, t, cx)),
                ),
        )
    }

    /// A copy or move in progress. The X stops it before the next file.
    fn render_file_task(&self, task: &FileTask, t: &Theme, cx: &Context<Self>) -> Div {
        let id = task.id;
        let stopping = task.progress.cancelled();
        let latest = self.latest_file_task().is_some_and(|latest| latest.id == id);
        div()
            .h(px(JOB_ROW_HEIGHT))
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .whitespace_nowrap()
            .text_size(px(11.))
            .text_color(t.text_faint)
            .child(self.spinner(t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .text_color(t.text)
                    .child(task.label()),
            )
            .map(|row| {
                if stopping {
                    row.child(div().flex_none().text_color(t.caret).child("stopping…"))
                } else if latest {
                    row.child(hint(t, &["Ctrl+C"], "stop").flex_none())
                } else {
                    row
                }
            })
            .child(
                div()
                    .flex_none()
                    .font_family(MONO_FONT)
                    .text_size(px(11.5))
                    .child(elapsed_text(task.started.elapsed())),
            )
            .child(close_button(
                t,
                ("stop-file-task", id as usize),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.cancel_file_task(id, cx);
                }),
            ))
    }

    /// A command's row. A click shows its output in the panel; the X stops it or, if it already
    /// ended, discards its result.
    fn render_job(&self, job: &Job, t: &Theme, cx: &Context<Self>) -> Stateful<Div> {
        let id = job.id;
        div()
            .id(("job", id as usize))
            .h(px(JOB_ROW_HEIGHT))
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .whitespace_nowrap()
            .text_size(px(11.))
            .text_color(t.text_faint)
            .cursor_pointer()
            .hover(|style| style.bg(t.raised_bg))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.select_job(id, cx);
            }))
            .map(|row| match &job.outcome {
                None => row
                    .child(self.spinner(t))
                    .child(command_label(t, job.command.clone()))
                    .children(self.render_job_state(job, t, cx)),
                Some(outcome) => row
                    .child(div().flex_none().w(px(14.)))
                    .child(command_label(t, job.command.clone()))
                    .child(
                        div()
                            .flex_none()
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .text_color(if outcome.failed {
                                t.caret
                            } else {
                                t.text_faint
                            })
                            .child(outcome.status.clone()),
                    )
                    .child(close_button(
                        t,
                        ("discard-job", id as usize),
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.close_job(id, cx);
                        }),
                    )),
            })
    }

    fn spinner(&self, t: &Theme) -> Svg {
        let angle = (self.spinner_frame % 8) as f32 * std::f32::consts::FRAC_PI_4;
        icon(icon::SPINNER, 14., t.command)
            .with_transformation(Transformation::rotate(radians(angle)))
    }

    /// State, elapsed time and X of a running command, for its row or the panel header.
    fn render_job_state(&self, job: &Job, t: &Theme, cx: &Context<Self>) -> Vec<AnyElement> {
        let id = job.id;
        let latest = self.latest_running() == Some(id);
        let state = match job.control.stopped() {
            None if latest => Some(hint(t, &["Ctrl+C"], "interrupt")),
            None => None,
            Some(_) => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_color(t.caret).child("interrupting…"))
                    .child(if latest {
                        hint(t, &["Ctrl+C"], "again to kill")
                    } else {
                        hint(t, &["×"], "again to kill")
                    }),
            ),
        };
        let mut elements = Vec::with_capacity(3);
        if let Some(state) = state {
            elements.push(state.flex_none().into_any_element());
        }
        elements.push(
            div()
                .flex_none()
                .font_family(MONO_FONT)
                .text_size(px(11.5))
                .child(elapsed_text(job.started.elapsed()))
                .into_any_element(),
        );
        elements.push(
            close_button(
                t,
                ("stop-job", id as usize),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.stop_job(id, cx);
                }),
            )
            .into_any_element(),
        );
        elements
    }

    /// Covers the whole window so file actions cannot receive clicks while asking.
    fn render_confirmation(&self, t: &Theme, cx: &Context<Self>) -> Option<Div> {
        if self.mode != Mode::Confirm {
            return None;
        }
        let paths = self.confirm_targets.as_deref().unwrap_or_default();
        let mut names: Vec<String> = paths
            .iter()
            .take(CONFIRM_NAMES)
            .map(|path| {
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        if paths.len() > CONFIRM_NAMES {
            names.push(format!("+{} more", paths.len() - CONFIRM_NAMES));
        }
        let question = if self.confirm_permanent {
            format!(
                "Are you sure you want to permanently delete {}?",
                items(paths.len())
            )
        } else {
            format!(
                "Are you sure you want to move {} to the trash?",
                items(paths.len())
            )
        };
        let confirm_label = if self.confirm_permanent {
            "Delete permanently"
        } else {
            "Move to trash"
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .key_context(CONFIRM_CONTEXT)
                .track_focus(&self.confirm_focus)
                .on_action(cx.listener(Self::confirm_delete))
                .on_action(cx.listener(Self::cancel_edit))
                .bg(gpui::rgba(0x00000099))
                .p(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(500.))
                        .max_w_full()
                        .p(px(24.))
                        .flex()
                        .flex_col()
                        .gap(px(18.))
                        .rounded(px(12.))
                        .bg(t.raised_bg)
                        .border_1()
                        .border_color(t.border_strong)
                        .child(
                            div()
                                .text_size(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_bright)
                                .child(question),
                        )
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_color(t.text_muted)
                                .child(names.join(", ")),
                        )
                        .child(
                            div()
                                .text_color(t.text_muted)
                                .child(if self.confirm_permanent {
                                    "This cannot be undone."
                                } else {
                                    "You can restore these items from the trash."
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(12.))
                                .child(
                                    div()
                                        .id("cancel-removal")
                                        .px(px(14.))
                                        .py(px(8.))
                                        .rounded(px(6.))
                                        .border_1()
                                        .border_color(t.border_strong)
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.leave_edit(window, cx)
                                        }))
                                        .child("Cancel (Esc)"),
                                )
                                .child(
                                    div()
                                        .id("confirm-removal")
                                        .px(px(14.))
                                        .py(px(8.))
                                        .rounded(px(6.))
                                        .bg(t.danger)
                                        .text_color(t.on_accent)
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm_delete(&ConfirmDelete, window, cx);
                                        }))
                                        .child(format!("{confirm_label} (Enter)")),
                                ),
                        ),
                ),
        )
    }

    fn render_status(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (mode_label, mode_color) = match self.mode {
            Mode::Normal if self.visual.is_some() => ("VISUAL", t.visual),
            Mode::Normal => ("NORMAL", t.accent),
            Mode::Insert => ("INSERT", t.caret),
            Mode::Command(_) => ("COMMAND", t.command),
            Mode::Search => ("SEARCH", t.search),
            Mode::Confirm => ("DELETE", t.danger),
        };
        let bar = div()
            .flex_none()
            .h(px(36.))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(12.))
            .overflow_hidden()
            .whitespace_nowrap()
            .border_t_1()
            .border_color(t.border)
            .text_size(px(11.))
            .text_color(t.text_faint)
            .child(
                div()
                    .flex_none()
                    .h(px(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(8.))
                    .bg(mode_color)
                    .text_color(t.on_accent)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(mode_label),
            );

        if self.mode == Mode::Search {
            let text = self.search_input.read(cx).text().to_owned();
            let count = (!text.is_empty()).then(|| match self.match_count(&text) {
                0 => div().flex_none().text_color(t.caret).child("no match"),
                1 => div().flex_none().child("1 match"),
                n => div().flex_none().child(format!("{n} matches")),
            });
            return bar
                .key_context(SEARCH_LINE_CONTEXT)
                .on_action(cx.listener(Self::confirm_search))
                .on_action(cx.listener(Self::cancel_edit))
                .on_action(cx.listener(Self::search_history_older))
                .on_action(cx.listener(Self::search_history_newer))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .font_family(MONO_FONT)
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_bright)
                        .child(
                            div()
                                .flex_none()
                                .text_color(t.search)
                                .font_weight(FontWeight::BOLD)
                                .child("/"),
                        )
                        .child(self.search_input.clone()),
                )
                .children(count);
        }

        if let Mode::Command(kind) = self.mode {
            let error = self.command_error.clone().map(|error| alert(t, error));
            return bar
                .key_context(COMMAND_LINE_CONTEXT)
                .on_action(cx.listener(Self::run_command))
                .on_action(cx.listener(Self::cancel_edit))
                .on_action(cx.listener(Self::history_older))
                .on_action(cx.listener(Self::history_newer))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .font_family(MONO_FONT)
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_bright)
                        .child(
                            div()
                                .flex_none()
                                .text_color(t.command)
                                .font_weight(FontWeight::BOLD)
                                .child(kind.prefix()),
                        )
                        .child(self.command_input.clone()),
                )
                .children(error);
        }

        let count = self.entries.len();
        let mut summary = if count == 1 {
            "1 item".to_owned()
        } else {
            format!("{count} items")
        };
        if let Some(Row::Entry(_)) = self.row(self.cursor) {
            let position = self.cursor + 1 - usize::from(self.has_parent());
            summary.push_str(&format!(" · {position}/{count}"));
        }
        let marked = (!self.marks.is_empty()).then(|| {
            div()
                .flex_none()
                .text_color(t.accent)
                .child(format!("{} marked", self.marks.len()))
        });
        let register = self.register_view.as_ref().map(|register| {
            let action = match register.op {
                fileops::Op::Copy => "to paste",
                fileops::Op::Move => "to move",
            };
            div()
                .flex_none()
                .text_color(t.text_muted)
                .child(format!("{} {action}", register.paths.len()))
        });
        let notice = self.notice.as_ref().map(|notice| {
            if notice.failed {
                alert(t, notice.text.clone())
                    .max_w(px(420.))
                    .text_size(px(11.))
            } else {
                div()
                    .flex_none()
                    .max_w(px(420.))
                    .truncate()
                    .text_color(t.text_muted)
                    .child(notice.text.clone())
            }
        });
        bar.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(MONO_FONT)
                .text_color(t.text_muted)
                .child(listing::display_path(&self.cwd, self.home.as_deref())),
        )
        .child(div().flex_none().child(summary))
        .children(marked)
        .children(register)
        .when(self.pending.is_some(), |bar| {
            bar.child(div().flex_none().text_color(t.caret).child("loading…"))
        })
        .children(notice)
    }
}

impl Focusable for Explorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.last_render = Instant::now();
        let register = self.register.load();
        let changed = match (&register, &self.register_view) {
            (Some(new), Some(old)) => !Arc::ptr_eq(new, old),
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.cut_paths = match &register {
                Some(register) if register.op == fileops::Op::Move => {
                    register.paths.iter().cloned().collect()
                }
                _ => HashSet::new(),
            };
        }
        self.register_view = register;
        let t = Theme::get(cx);
        let show_preview = window.viewport_size().width >= px(PREVIEW_MIN_WINDOW_WIDTH);
        if show_preview || self.quick_look {
            self.request_preview(window, cx);
        }

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text)
            .font_family(UI_FONT)
            .text_size(px(13.))
            .line_height(relative(1.4))
            .child(self.render_header(&t, window, cx))
            .child(
                div()
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::move_down))
                    .on_action(cx.listener(Self::move_up))
                    .on_action(cx.listener(Self::first))
                    .on_action(cx.listener(Self::last))
                    .on_action(cx.listener(Self::open))
                    .on_action(cx.listener(Self::parent))
                    .on_action(cx.listener(Self::edit_path))
                    .on_action(cx.listener(Self::enter_command))
                    .on_action(cx.listener(Self::enter_launch))
                    .on_action(cx.listener(Self::enter_search))
                    .on_action(cx.listener(Self::next_match))
                    .on_action(cx.listener(Self::previous_match))
                    .on_action(cx.listener(Self::toggle_quick_look))
                    .on_action(cx.listener(Self::toggle_mark))
                    .on_action(cx.listener(Self::toggle_visual))
                    .on_action(cx.listener(Self::extend_down))
                    .on_action(cx.listener(Self::extend_up))
                    .on_action(cx.listener(Self::mark_all))
                    .on_action(cx.listener(Self::yank))
                    .on_action(cx.listener(Self::cut))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::trash))
                    .on_action(cx.listener(Self::ask_delete))
                    .on_action(cx.listener(Self::close_output))
                    .on_action(cx.listener(Self::interrupt))
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .flex()
                            .child(self.render_list(&t, cx))
                            .when(show_preview, |body| body.child(self.render_preview(&t)))
                            .children(self.render_quick_look(&t)),
                    )
                    .children(self.render_jobs(&t, cx))
                    .when_some(self.shown_job(), |body, job| {
                        body.child(self.render_output(job, &t, window, cx))
                    }),
            )
            .child(self.render_status(&t, cx))
            .children(self.render_completion(&t))
            .children(self.render_confirmation(&t, cx))
    }
}

/// Path segments that fit in `budget` characters. If not all of them fit, the first ones are
/// dropped and `…` is added. `rooted` means the path starts with `/`.
fn breadcrumb(display: &str, budget: usize) -> (bool, Vec<&str>) {
    if display == "/" {
        return (false, vec!["/"]);
    }
    let rooted = display.starts_with('/');
    let mut segments: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    // Each separator takes about three characters with its spacing.
    let cost =
        |segments: &[&str]| -> usize { segments.iter().map(|s| s.chars().count() + 3).sum() };
    if cost(&segments) > budget {
        while segments.len() > 1 && cost(&segments) + 4 > budget {
            segments.remove(0);
        }
        segments.insert(0, "…");
        return (false, segments);
    }
    (rooted, segments)
}

/// Type, dimensions (for an image with a thumbnail) and size of a file; just the type for a folder.
fn entry_subtitle(
    entry: &Entry,
    thumbnail: Option<&Result<preview::Thumbnail, preview::Failure>>,
) -> SharedString {
    match thumbnail {
        _ if entry.is_dir() => entry.type_text.clone(),
        Some(Ok(thumbnail)) => format!(
            "{} · {} × {} · {}",
            entry.type_text, thumbnail.width, thumbnail.height, entry.size_text
        )
        .into(),
        _ => format!("{} · {}", entry.type_text, entry.size_text).into(),
    }
}

/// An entry name with the matches of `pattern` highlighted.
fn highlighted_name(t: &Theme, name: SharedString, pattern: Option<&str>) -> StyledText {
    let ranges = pattern.map_or_else(Vec::new, |pattern| search::match_ranges(&name, pattern));
    let style = HighlightStyle {
        color: Some(t.text_bright),
        background_color: Some(t.match_bg),
        ..HighlightStyle::default()
    };
    StyledText::new(name).with_highlights(ranges.into_iter().map(|range| (range, style)))
}

/// `! command`, cut with `…` if it does not fit.
fn command_label(t: &Theme, command: SharedString) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(8.))
        .font_family(MONO_FONT)
        .text_size(px(12.))
        .child(
            div()
                .flex_none()
                .text_color(t.command)
                .font_weight(FontWeight::BOLD)
                .child("!"),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(t.text_bright)
                .child(command),
        )
}

fn close_button(
    t: &Theme,
    id: impl Into<ElementId>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|style| style.bg(t.border_strong))
        .on_click(on_click)
        .child(icon(icon::CLOSE, 14., t.text_muted))
}

/// Index and length in characters of the longest line; `(0, 0)` if there is none. Among equals,
/// the newest: it is the last to be dropped by the limit.
fn longest_line<'a>(lines: impl Iterator<Item = (usize, &'a SharedString)>) -> (usize, usize) {
    lines
        .map(|(index, line)| (index, line.chars().count()))
        .max_by_key(|&(index, length)| (length, index))
        .unwrap_or((0, 0))
}

/// Elapsed time of a running command: `4s`, `1m 12s`, `2h 5m`.
/// The notice after an operation: `Moved 3 items to the trash`, `Pasted 2 of 5 items · stopped ·
/// dir: cannot paste a folder into itself`. Whether something failed.
fn report_text(verb: &str, place: &str, total: usize, report: &fileops::Report) -> (String, bool) {
    let done = report.done.len();
    let mut text = if done == total {
        format!("{verb} {}{place}", items(total))
    } else {
        format!("{verb} {done} of {}{place}", items(total))
    };
    if !report.skipped.is_empty() {
        text.push_str(" · stopped");
    }
    if let Some(failure) = report.failures.first() {
        let name = failure.path.file_name().map_or_else(
            || failure.path.to_string_lossy(),
            |name| name.to_string_lossy(),
        );
        text.push_str(&format!(" · {name}: {}", failure.reason));
        if report.failures.len() > 1 {
            text.push_str(&format!(" (+{} more)", report.failures.len() - 1));
        }
    }
    (text, !report.failures.is_empty())
}

/// `1 item`, `3 items`.
fn items(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

fn icon(path: &'static str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(path)
        .flex_none()
        .size(px(size))
        .text_color(color)
}

/// Amber notice with an icon, like the typed-path error.
fn alert(t: &Theme, text: SharedString) -> Div {
    div()
        .flex_none()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .font_family(UI_FONT)
        .text_size(px(12.))
        .font_weight(FontWeight::NORMAL)
        .text_color(t.caret)
        .child(icon(icon::ALERT, 14., t.caret))
        .child(div().min_w_0().truncate().child(text))
}

fn section_title(t: &Theme, text: &'static str) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_faint)
        .child(text)
}

fn card(t: &Theme, label: &'static str, value: SharedString, mono: bool) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .px(px(12.))
        .py(px(10.))
        .rounded(px(10.))
        .border_1()
        .border_color(t.border)
        .child(
            div()
                .mb(px(2.))
                .text_size(px(11.))
                .text_color(t.text_faint)
                .child(label),
        )
        .child(
            div()
                .truncate()
                .text_color(t.text)
                .map(|value| {
                    if mono {
                        value.font_family(MONO_FONT).text_size(px(11.5))
                    } else {
                        value.text_size(px(12.))
                    }
                })
                .child(value),
        )
}

fn kbd(t: &Theme, key: &'static str) -> Div {
    div()
        .px(px(7.))
        .py(px(3.))
        .rounded_full()
        .border_1()
        .border_color(t.border_strong)
        .bg(t.raised_bg)
        .font_family(MONO_FONT)
        .text_size(px(10.5))
        .line_height(relative(1.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_muted)
        .child(key)
}

fn hint(t: &Theme, keys: &[&'static str], label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .children(keys.iter().map(|key| kbd(t, key)))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_elapsed_time() {
        assert_eq!(elapsed_text(Duration::from_millis(400)), "0s");
        assert_eq!(elapsed_text(Duration::from_secs(4)), "4s");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed_text(Duration::from_secs(72)), "1m 12s");
        assert_eq!(
            elapsed_text(Duration::from_secs(3600 * 2 + 5 * 60 + 9)),
            "2h 5m"
        );
    }

    #[test]
    fn breadcrumb_keeps_short_paths() {
        assert_eq!(
            breadcrumb("~/projects/app", 80),
            (false, vec!["~", "projects", "app"])
        );
        assert_eq!(breadcrumb("/usr/lib", 80), (true, vec!["usr", "lib"]));
        assert_eq!(breadcrumb("/", 80), (false, vec!["/"]));
    }

    #[test]
    fn breadcrumb_drops_leading_segments_when_long() {
        let path = "~/aaaaaaaaaa/bbbbbbbbbb/cccccccccc/dddd";
        let (rooted, segments) = breadcrumb(path, 20);
        assert!(!rooted);
        assert_eq!(segments, vec!["…", "dddd"]);
        let (_, segments) = breadcrumb(path, 24);
        assert_eq!(segments, vec!["…", "cccccccccc", "dddd"]);
        let (_, segments) = breadcrumb(path, 40);
        assert_eq!(segments, vec!["…", "bbbbbbbbbb", "cccccccccc", "dddd"]);
    }
}

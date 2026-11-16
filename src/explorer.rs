//! Main view: path bar, list, preview and status bar, on a single background and separated by
//! 1 px lines.
//!
//! It has four modes. In `Normal` the keyboard navigates the list; `i` switches to `Insert`,
//! which edits the path in the top bar, `!` or `&` to `Command`, which types a shell command in
//! the status bar, and `/` to `Search`, which moves the cursor to the entries that match.
//! `Space` opens a quick look over the list with a large preview of the entry under the cursor. `Escape` or losing focus go back to `Normal`. The output of `!` commands
//! shows in a panel between the list and the status bar; `&` commands are launched detached,
//! without output.

use std::collections::VecDeque;
use std::ffi::OsString;
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
use crate::listing::{self, Entry};
use crate::preview;
use crate::search::{self, Direction};
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
        CancelEdit,
        EnterCommand,
        EnterLaunch,
        EnterSearch,
        ConfirmSearch,
        NextMatch,
        PreviousMatch,
        ToggleQuickLook,
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

pub fn bind_keys(cx: &mut App) {
    let list = Some(LIST_CONTEXT);
    let path_bar = Some(PATH_BAR_CONTEXT);
    let command_line = Some(COMMAND_LINE_CONTEXT);
    let search_line = Some(SEARCH_LINE_CONTEXT);
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
        KeyBinding::new("escape", CloseOutput, list),
        // In text fields `ctrl-c` is still copy: this context does not cover them.
        KeyBinding::new("ctrl-c", Interrupt, list),
        KeyBinding::new("enter", ConfirmPath, path_bar),
        KeyBinding::new("escape", CancelEdit, path_bar),
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
        let subscriptions = vec![
            cx.subscribe(&path_input, |this, _, _: &text_input::Changed, cx| {
                if this.path_error.take().is_some() {
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
        ];
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut explorer = Self {
            focus,
            mode: Mode::Normal,
            path_input,
            path_error: None,
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
        self.path_error = None;
        window.focus(&self.path_input.focus_handle(cx));
        cx.notify();
    }

    /// Looks up the typed path in the background and, if it exists, goes there.
    fn confirm_path(&mut self, _: &ConfirmPath, window: &mut Window, cx: &mut Context<Self>) {
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
        self.leave_edit(window, cx);
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
                    let animating = this
                        .jobs
                        .iter()
                        .any(|job| job.running() && (job.visible || this.shown == Some(job.id)));
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

    /// `Ctrl+C`: stops the most recent command.
    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        match self.latest_running() {
            Some(id) => self.stop_job(id, cx),
            None => cx.propagate(),
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
        if self.quick_look {
            self.quick_look = false;
            cx.notify();
            return;
        }
        let Some(id) = self.shown.take() else {
            if self.search_highlight {
                self.search_highlight = false;
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
        self.set_cursor(index, cx);
        if event.click_count() >= 2 {
            self.open_cursor(cx);
        }
    }

    fn render_header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let bar = div()
            .flex_none()
            .h(px(52.))
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
                } else {
                    row.border_color(transparent_black())
                        .hover(|style| style.bg(t.raised_bg))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                this.click_row(index, event, cx);
            }))
            .child(div().flex_none().w(px(22.)));

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
        if visible.is_empty() {
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
                .children(rows),
        )
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

    fn render_status(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (mode_label, mode_color) = match self.mode {
            Mode::Normal => ("NORMAL", t.accent),
            Mode::Insert => ("INSERT", t.caret),
            Mode::Command(_) => ("COMMAND", t.command),
            Mode::Search => ("SEARCH", t.search),
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
        let hints = |hints: Vec<Div>| {
            div()
                .flex_none()
                .pr(px(6.))
                .flex()
                .items_center()
                .gap(px(10.))
                .children(hints)
        };

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
                .children(count)
                .child(hints(vec![
                    hint(t, &["Enter"], "go"),
                    hint(t, &["↑", "↓"], "history"),
                    hint(t, &["Esc"], "cancel"),
                ]));
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
                .children(error)
                .child(hints(vec![
                    hint(
                        t,
                        &["Enter"],
                        match kind {
                            CommandKind::Run => "run",
                            CommandKind::Launch => "launch",
                        },
                    ),
                    hint(t, &["↑", "↓"], "history"),
                    hint(t, &["%s"], "file"),
                    hint(t, &["Esc"], "cancel"),
                ]));
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
        let mode_hints = match self.mode {
            Mode::Insert => vec![hint(t, &["Enter"], "go"), hint(t, &["Esc"], "cancel")],
            _ => vec![
                hint(t, &["j", "k"], "move"),
                hint(t, &["l"], "open"),
                hint(t, &["h"], "back"),
                hint(t, &["gg", "G"], "top/bottom"),
                hint(t, &["i"], "path"),
                hint(t, &["!"], "command"),
                hint(t, &["&"], "launch"),
                hint(t, &["/"], "search"),
                hint(t, &["Space"], "preview"),
            ],
        };

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
        .when(self.pending.is_some(), |bar| {
            bar.child(div().flex_none().text_color(t.caret).child("loading…"))
        })
        .children(notice)
        .child(hints(mode_hints))
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
        let t = Theme::get(cx);
        let show_preview = window.viewport_size().width >= px(PREVIEW_MIN_WINDOW_WIDTH);
        if show_preview || self.quick_look {
            self.request_preview(window, cx);
        }

        div()
            .size_full()
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

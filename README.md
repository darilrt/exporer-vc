# exporer

A keyboard-first file explorer for Linux, written in Rust with
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), the UI framework behind the
Zed editor.

> **Status:** early prototype. It browses folders, edits paths, runs shell commands and copies,
> moves and deletes files. Opening files from the list and settings are not there yet.

> **Vibe-coded:** this project is written by prompting AI coding assistants, with a human deciding
> what to build and testing the result. Keep that in mind when reading the code.

## Features

- **Vim-style navigation:** `j`/`k` to move, `l` to enter a folder, `h` to go up, `gg`/`G` to jump
  to the first or last row. Going up leaves the cursor on the folder you came from.
- **Search** the current folder with `/`, as in vim: the cursor jumps to the first match as you
  type and the matches are highlighted; `n`/`N` go to the next or previous one. Matching ignores
  case unless the search has an uppercase letter.
- **Edit the path** with `i`: type an absolute path, a relative one, `~` or `..`, and press `Enter`.
  Typing a file's path opens its folder with the file selected. `Tab` completes the last segment
  as a shell does: the part all matches share, or the whole name if there is only one (folders end
  in `/`). With several matches a list opens under the bar; `Tab`/`Shift+Tab` (or `↓`/`↑`) go
  through it and `Esc` closes it. Matching ignores case unless you type an uppercase letter.
- **Run shell commands** in the current folder with `!`. The output shows live in a panel below the
  list, follows the end like a terminal and keeps the last 1 MiB per command. `%s` expands to the
  entry under the cursor (quoted for the shell) and `↑`/`↓` walk the history.
- **Stop commands:** a running command shows a row with a spinner, its elapsed time and an `×`.
  `Ctrl+C` sends `SIGINT` to the command's whole process group, as a terminal would; pressing it
  again sends `SIGKILL`.
- **Launch programs detached** with `&`, for example `& code .` or `& xdg-open %s`. They run in a
  new session with no terminal and no connection to exporer, so they keep running after you close
  it.
- **Preview panel** with the entry's type, size, modification date and permissions, and a preview
  of images: PNG, JPEG, GIF, WebP, BMP, TIFF, SVG and other formats, turned upright from their
  EXIF orientation. Thumbnails are decoded in the background and only the last few are kept in
  memory.
- **Mark entries** with `Tab`, a visual selection (`v`, then move and `v` again, or
  `Shift+↑`/`Shift+↓`), `Ctrl+A`, or `Ctrl`/`Shift`+click.
  Marks are full paths, so they stay when you move to another folder and one operation can take
  entries from several folders. Without marks, operations act on the entry under the cursor.
- **Copy, cut and paste** with `yy`, `xx` and `pp`, or `Ctrl+C`, `Ctrl+X` and `Ctrl+V`. The register
  is shared by every exporer window of the session (`$XDG_RUNTIME_DIR/exporer/register`), so you
  can copy in one window and paste in another. Nothing is ever overwritten: a taken name becomes `name (1).ext`. Pasting runs in the
  background with a progress row that `Ctrl+C` or its `×` stops; moving across file systems copies
  first and removes the original only if the copy was complete.
- **Delete** with `dd`, which asks for confirmation and moves entries to the trash (the freedesktop.org trash that Dolphin and
  Nautilus use, so they can be restored from there), or `D`, which deletes them for good after
  asking in a confirmation dialog.
- **Quick look** with `Space`: a large preview of the entry under the cursor floats over the list,
  whatever the window size, and follows the cursor as you move.

## Cheat sheet

### Moving around

| Key                     | Action                                                   |
| ----------------------- | -------------------------------------------------------- |
| `j` / `↓`               | Move down                                                |
| `k` / `↑`               | Move up                                                  |
| `l` / `→` / `Enter`     | Open the folder under the cursor                         |
| `h` / `←` / `Backspace` | Go to the parent folder (the cursor lands where you were) |
| `gg` / `Home`           | First row                                                |
| `G` / `End`             | Last row                                                 |
| `i`                     | Type a path (see [Path bar](#path-bar))                  |
| `Space`                 | Open or close the quick look                             |
| `Ctrl+Q`                | Quit                                                     |

### Search

| Key       | Action                                                             |
| --------- | ------------------------------------------------------------------ |
| `/`       | Search the current folder; the cursor jumps as you type            |
| `Enter`   | Keep the cursor on the match                                       |
| `Esc`     | Cancel and go back to where the cursor was                         |
| `↑` / `↓` | Search history (while typing)                                      |
| `n` / `N` | Next / previous match                                              |

Matching ignores case unless the search has an uppercase letter.

### Selecting

| Key                   | Action                                                            |
| --------------------- | ----------------------------------------------------------------- |
| `Tab`                 | Mark or unmark the entry under the cursor and move down           |
| `v`                   | Start a visual selection; move, then `v` again to mark the rows   |
| `Shift+↑` / `Shift+↓` | Start or extend a visual selection                                |
| `Ctrl+A`              | Mark every entry in the folder                                    |
| `Esc`                 | Drop the visual selection, or clear the marks                     |
| `Ctrl`+click          | Mark or unmark a row                                              |
| `Shift`+click         | Mark the rows from the cursor to the clicked one                  |

Marks stay when you change folder. Without marks, operations act on the entry under the cursor.

### Files

| Key               | Action                                                                     |
| ----------------- | -------------------------------------------------------------------------- |
| `yy` / `Ctrl+C`    | Copy the selection                                                         |
| `xx` / `Ctrl+X`    | Cut the selection                                                          |
| `pp` / `Ctrl+V`    | Paste into the current folder (a taken name becomes `name (1).ext`)        |
| `dd`               | Move to the trash after confirmation                                       |
| `D`               | Delete permanently; `y` / `Enter` confirms, `n` / `Esc` cancels            |
| `Ctrl+C` / `×`    | Stop a paste or delete in progress                                         |

`Ctrl+C` copies only when nothing is running in view; otherwise it interrupts. The register is
shared between exporer windows, so you can copy in one and paste in another.

### Path bar

Press `i` to type a path: absolute, relative, `~` or `..`. A file's path opens its folder with the
file selected.

| Key                         | Action                                                       |
| --------------------------- | ------------------------------------------------------------ |
| `Enter`                     | Go there                                                     |
| `Tab`                       | Complete the last segment, as a shell does                   |
| `Tab` / `↓`, `Shift+Tab` / `↑` | Go through the list of completions                        |
| `Ctrl+Backspace` / `Ctrl+W` | Delete the last segment                                      |
| `Esc`                       | Close the completions, or cancel                             |

### Shell commands

| Key       | Action                                                                  |
| --------- | ----------------------------------------------------------------------- |
| `!`       | Run a command in the current folder; its output shows in a panel        |
| `&`       | Launch a program detached (it keeps running after exporer closes)       |
| `Enter`   | Run it                                                                  |
| `↑` / `↓` | Command history                                                         |
| `Esc`     | Cancel the command line; in the list, close the output panel           |
| `Ctrl+C`  | Interrupt the command in view (`SIGINT`); again to kill it (`SIGKILL`)  |

On the command line, `%s` is the entry under the cursor, quoted for the shell, and `%%` is a
literal `%`. Some examples:

| Command               | What it does                                   |
| --------------------- | ---------------------------------------------- |
| `! ls -la`            | List the folder with details                   |
| `! du -sh %s`         | Size of the entry under the cursor             |
| `! git status`        | Git status of the current folder               |
| `! mkdir new-folder`  | Create a folder                                |
| `! touch notes.txt`   | Create an empty file                           |
| `! mv %s new-name`    | Rename the entry under the cursor              |
| `! date +%%s`         | A literal `%` is written `%%`                  |
| `& xdg-open %s`       | Open the entry with its default application    |
| `& code .`            | Open the folder in an editor                   |
| `& kitty`             | Open a terminal here                           |

Clicking a command's row shows its output; its `×` stops it or discards a finished result.

### Text fields

All fields (path, search, command line) support `←` / `→`, `Home` / `End`, selection with
`Shift+←` / `Shift+→` or the mouse, `Ctrl+A`, `Ctrl+C` / `Ctrl+X` / `Ctrl+V` with the system
clipboard, `Backspace`, `Delete` and `Ctrl+Backspace` / `Ctrl+W` to delete a word.

### Mouse

Click selects a row, double-click opens a folder, the wheel scrolls, `Ctrl`+click and
`Shift`+click mark rows.

### Escape

`Esc` closes one thing at a time, in this order: the visual selection, the quick look, the output
panel, the search highlight and the marks.

## Building

You need:

- A Rust toolchain with edition 2024 support (tested with Rust 1.97).
- Linux with Wayland or X11 and a Vulkan-capable graphics driver.
- The development files for xcb and xkbcommon (on Debian or Ubuntu: `libxcb1-dev`,
  `libxkbcommon-dev` and `libxkbcommon-x11-dev`).

```sh
cargo build --release
./target/release/exporer [PATH]
```

`PATH` is the folder to open, or a file to open its folder with that file selected. It defaults to
the current directory.

`cargo test` runs the test suite, which starts real `sh` processes.

## Notes

- Commands run with `sh -c` in the folder shown, with standard input set to `/dev/null`. There is no
  terminal, so interactive programs (`vim`, `less`, password prompts) do not work with `!`; launch a
  terminal with `&` instead.
- Output is shown as plain text: colors and other escape sequences are stripped, and carriage
  returns update the line in place, so progress bars stay on one line.
- exporer does not watch folders: changes made elsewhere show when you come back to the window, or
  when the folder is read again.
- Only Linux is supported for now: the command handling relies on Unix process groups and sessions.

## Credits

- [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) by Zed Industries, under the
  Apache License 2.0.
- [Inter](https://github.com/rsms/inter) and [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono),
  embedded in the binary, under the SIL Open Font License 1.1. Their license texts are in
  `assets/fonts`.

## License

exporer is released under the [MIT License](LICENSE). The bundled fonts keep their own licenses
(see Credits).

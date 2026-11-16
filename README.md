# exporer

A keyboard-first file explorer for Linux, written in Rust with
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), the UI framework behind the
Zed editor.

> **Status:** early prototype. It browses folders, edits paths and runs shell commands. Opening files
> from the list, file operations and settings are not there yet.

> **Vibe-coded:** this project is written by prompting AI coding assistants, with a human deciding
> what to build and testing the result. Keep that in mind when reading the code.

## Features

- **Vim-style navigation:** `j`/`k` to move, `l` to enter a folder, `h` to go up, `gg`/`G` to jump
  to the first or last row. Going up leaves the cursor on the folder you came from.
- **Search** the current folder with `/`, as in vim: the cursor jumps to the first match as you
  type and the matches are highlighted; `n`/`N` go to the next or previous one. Matching ignores
  case unless the search has an uppercase letter.
- **Edit the path** with `i`: type an absolute path, a relative one, `~` or `..`, and press `Enter`.
  Typing a file's path opens its folder with the file selected.
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
- **Quick look** with `Space`: a large preview of the entry under the cursor floats over the list,
  whatever the window size, and follows the cursor as you move.

## Keys

| Key                     | Action                                                                  |
| ----------------------- | ----------------------------------------------------------------------- |
| `j` / `↓`               | Move down                                                               |
| `k` / `↑`               | Move up                                                                 |
| `l` / `→` / `Enter`     | Open the folder under the cursor                                        |
| `h` / `←` / `Backspace` | Go to the parent folder                                                 |
| `gg` / `Home`           | First row                                                               |
| `G` / `End`             | Last row                                                                |
| `/`                     | Search the current folder                                               |
| `n` / `N`               | Next / previous match                                                   |
| `Space`                 | Open or close the quick look                                            |
| `i`                     | Edit the path                                                           |
| `!`                     | Run a shell command                                                     |
| `&`                     | Launch a program detached                                               |
| `Ctrl+C`                | Interrupt the most recent command (again to kill)                       |
| `Esc`                   | Close the quick look or the output panel, or clear the search highlight |
| `Ctrl+Q`                | Quit                                                                    |

While searching, `Enter` keeps the cursor on the match, `Esc` goes back to where it was and `↑`/`↓`
walk the search history. While editing the path, `Enter` goes there and `Esc` cancels. On the
command line (`!` or `&`), `Enter` runs it, `↑`/`↓` walk the history, `%s` is the entry under the
cursor, `%%` is a literal `%` and `Esc` cancels. Text fields support selection with `Shift`,
`Ctrl+A` and the clipboard.

With the mouse: click selects a row, double-click opens a folder, the wheel scrolls, clicking a
command's row shows its output in the panel, and its `×` stops it (or discards a finished result).

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

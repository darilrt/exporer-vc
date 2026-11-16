use gpui::{App, Global, Hsla, rgb, rgba};

/// Colors of the reference design as tokens. It is a `Global` so the theme can change without
/// touching the views.
#[derive(Clone, Copy)]
pub struct Theme {
    /// The single background of the window. Areas are separated by `border` lines, not by
    /// another background color.
    pub bg: Hsla,
    /// Background of raised elements: keys, hover.
    pub raised_bg: Hsla,
    /// Background of the row under the cursor.
    pub cursor_bg: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_bright: Hsla,
    pub path_segment: Hsla,
    pub path_separator: Hsla,
    /// Text field caret and insert mode; also warnings.
    pub caret: Hsla,
    /// Background of selected text in a text field.
    pub selection: Hsla,
    pub accent: Hsla,
    /// Command mode (`!`) and the command prefix.
    pub command: Hsla,
    /// Search mode (`/`) and its prefix.
    pub search: Hsla,
    /// Background of the parts of a name that match the search.
    pub match_bg: Hsla,
    /// Text on top of the accent color.
    pub on_accent: Hsla,
}

impl Global for Theme {}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x060708).into(),
            raised_bg: rgb(0x16181C).into(),
            cursor_bg: rgb(0x1C1F25).into(),
            border: rgb(0x23262C).into(),
            border_strong: rgb(0x2C3038).into(),
            text: rgb(0xF2F3F5).into(),
            text_muted: rgb(0xAEB2BC).into(),
            text_faint: rgb(0x7F8491).into(),
            text_bright: rgb(0xFFFFFF).into(),
            path_segment: rgb(0xB3B3B3).into(),
            path_separator: rgb(0x6B6B6B).into(),
            caret: rgb(0xE0AF68).into(),
            selection: rgba(0xA78BFA59).into(),
            accent: rgb(0xA78BFA).into(),
            command: rgb(0x7AA2F7).into(),
            search: rgb(0x9ECE6A).into(),
            match_bg: rgba(0xE0AF6866).into(),
            on_accent: rgb(0x060708).into(),
        }
    }

    pub fn get(cx: &App) -> Self {
        *cx.global::<Self>()
    }
}

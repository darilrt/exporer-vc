mod assets;
mod complete;
mod explorer;
mod fileops;
mod listing;
mod preview;
mod search;
mod selection;
mod shell;
mod text_input;
mod theme;

use std::ffi::OsString;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::explorer::{Explorer, Quit};
use crate::theme::Theme;

/// Starting folder: the one given as an argument or the current directory. If the argument is a
/// file, its folder opens with the file under the cursor.
fn start_location() -> (PathBuf, Option<OsString>) {
    let requested = std::env::args_os().nth(1).map(PathBuf::from);
    let resolved = requested.as_ref().and_then(|path| path.canonicalize().ok());
    if let (Some(path), None) = (&requested, &resolved) {
        eprintln!("exporer: {} not found", path.display());
    }
    let path = resolved
        .or_else(|| std::env::current_dir().ok())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    if path.is_dir() {
        return (path, None);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => (parent.to_path_buf(), Some(name.to_os_string())),
        _ => (PathBuf::from("/"), None),
    }
}

fn main() {
    let (start, select) = start_location();

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(error) = assets::load_fonts(cx) {
                eprintln!("exporer: could not load the fonts: {error:#}");
            }
            cx.set_global(Theme::dark());
            explorer::bind_keys(cx);
            text_input::bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("exporer".into()),
                    ..Default::default()
                }),
                app_id: Some("exporer".to_owned()),
                window_min_size: Some(size(px(720.), px(480.))),
                ..Default::default()
            };
            let opened = cx.open_window(options, |window, cx| {
                cx.new(|cx| Explorer::new(start, select, window, cx))
            });
            if let Err(error) = opened {
                eprintln!("exporer: could not open the window: {error:#}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

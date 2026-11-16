//! Resources embedded in the binary: SVG icons and fonts.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

pub mod icon {
    pub const ALERT: &str = "icons/alert.svg";
    pub const CLOSE: &str = "icons/close.svg";
    #[allow(dead_code, reason = "only used by the Places sidebar, hidden for now")]
    pub const DOCUMENT: &str = "icons/document.svg";
    #[allow(dead_code, reason = "only used by the Places sidebar, hidden for now")]
    pub const DOWNLOAD: &str = "icons/download.svg";
    pub const FILE: &str = "icons/file.svg";
    pub const FOLDER: &str = "icons/folder.svg";
    pub const FOLDER_FILL: &str = "icons/folder-fill.svg";
    #[allow(dead_code, reason = "only used by the Places sidebar, hidden for now")]
    pub const HOME: &str = "icons/home.svg";
    pub const IMAGE: &str = "icons/image.svg";
    #[allow(dead_code, reason = "only used by the Places sidebar, hidden for now")]
    pub const MUSIC: &str = "icons/music.svg";
    pub const PARENT: &str = "icons/parent.svg";
    /// Eight spokes with decreasing opacity: rotated in 45° steps, it looks like it spins.
    pub const SPINNER: &str = "icons/spinner.svg";
    #[allow(dead_code, reason = "only used by the Places sidebar, hidden for now")]
    pub const VIDEO: &str = "icons/video.svg";
}

macro_rules! embedded {
    ($($path:expr),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../assets/", $path)))),*]
    };
}

const ICONS: &[(&str, &[u8])] = embedded![
    "icons/alert.svg",
    "icons/close.svg",
    "icons/document.svg",
    "icons/download.svg",
    "icons/file.svg",
    "icons/folder.svg",
    "icons/folder-fill.svg",
    "icons/home.svg",
    "icons/image.svg",
    "icons/music.svg",
    "icons/parent.svg",
    "icons/spinner.svg",
    "icons/video.svg",
];

const FONTS: &[(&str, &[u8])] = embedded![
    "fonts/Inter-Regular.ttf",
    "fonts/Inter-Medium.ttf",
    "fonts/Inter-SemiBold.ttf",
    "fonts/JetBrainsMono-Regular.ttf",
    "fonts/JetBrainsMono-Medium.ttf",
    "fonts/JetBrainsMono-Bold.ttf",
];

pub const UI_FONT: &str = "Inter";
pub const MONO_FONT: &str = "JetBrains Mono";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

/// Registers the embedded fonts without copying them: text reads them from the binary.
pub fn load_fonts(cx: &App) -> Result<()> {
    cx.text_system().add_fonts(
        FONTS
            .iter()
            .map(|(_, bytes)| Cow::Borrowed(*bytes))
            .collect(),
    )
}

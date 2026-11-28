//! The look (issue #54): paper for words, ink for work, vermilion for
//! "now" and "needs you". One module owns the palette, the two kit
//! themes built from it (Night, the default, and Day), and the embedded
//! fonts, so every kit widget and every gertie surface paints from the
//! same values.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, Hsla, SharedString, rgb};
use std::rc::Rc;

/// The prose and heading face: assistant replies, titles, captions.
pub const SERIF: &str = "Shippori Mincho";
/// The interface face: labels, buttons, the composer's furniture.
pub const SANS: &str = "Inter";
/// Code, paths, commands.
pub const MONO: &str = "IBM Plex Mono";

/// Gertie's own tokens, beyond what the kit theme names. Read through
/// [`palette`], which picks the set for the current mode.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// The rail's ground.
    pub rail: Hsla,
    /// The words pane.
    pub paper: Hsla,
    /// Ask cards and the composer on paper.
    pub paper_raised: Hsla,
    /// The work pane.
    pub ink: Hsla,
    /// Cards on ink.
    pub ink_raised: Hsla,
    /// The ensō's disc.
    pub disc: Hsla,
    /// Dividers on paper.
    pub line: Hsla,
    /// Dividers on ink.
    pub ink_line: Hsla,
    /// Outlines of controls on paper.
    pub control: Hsla,
    /// Primary text on paper.
    pub text: Hsla,
    /// Captions on paper.
    pub text_muted: Hsla,
    /// The user's message and earlier text.
    pub text_past: Hsla,
    /// Primary text on ink.
    pub ink_text: Hsla,
    /// Captions on ink.
    pub ink_muted: Hsla,
    /// Diff text and output on ink.
    pub ink_body: Hsla,
    /// Now / needs you: marks, flags, the ensō's head.
    pub vermilion: Hsla,
    /// The memory seal's fill.
    pub seal: Hsla,
    /// Text on the seal.
    pub seal_text: Hsla,
    /// Additions, passes, links on ink.
    pub added: Hsla,
    /// Removals.
    pub removed: Hsla,
    /// Links on paper.
    pub link: Hsla,
    /// The solid control: the send button, the primary choice.
    pub solid: Hsla,
    /// Text on the solid control.
    pub solid_text: Hsla,
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

pub fn night() -> Palette {
    Palette {
        rail: hex(0x1A1917),
        paper: hex(0x23211E),
        paper_raised: hex(0x2A2824),
        ink: hex(0x161513),
        ink_raised: hex(0x1E1D1A),
        disc: hex(0x2C2A26),
        line: hex(0x34312C),
        ink_line: hex(0x2E2C28),
        control: hex(0x4A463F),
        text: hex(0xECE7DC),
        text_muted: hex(0xA8A296),
        text_past: hex(0x8E887C),
        ink_text: hex(0xEDE9DF),
        ink_muted: hex(0xA39E92),
        ink_body: hex(0xB9B4A9),
        vermilion: hex(0xE0704F),
        seal: hex(0xB5432A),
        seal_text: hex(0xF6EFE4),
        added: hex(0xA9C4E8),
        removed: hex(0xE59A86),
        link: hex(0xA9C4E8),
        solid: hex(0xECE7DC),
        solid_text: hex(0x1A1917),
    }
}

pub fn day() -> Palette {
    Palette {
        rail: hex(0xF3F0E8),
        paper: hex(0xF3F0E8),
        paper_raised: hex(0xFBFAF6),
        ink: hex(0x1C1B19),
        ink_raised: hex(0x23221F),
        disc: hex(0xF3F0E8),
        line: hex(0xE2DDD1),
        ink_line: hex(0x34312C),
        control: hex(0xCFC8B8),
        text: hex(0x1D1C1A),
        text_muted: hex(0x6B675E),
        text_past: hex(0x8A857A),
        ink_text: hex(0xEDE9DF),
        ink_muted: hex(0xA39E92),
        ink_body: hex(0xB9B4A9),
        vermilion: hex(0xB5432A),
        seal: hex(0xB5432A),
        seal_text: hex(0xF3F0E8),
        added: hex(0xA9C4E8),
        removed: hex(0xE59A86),
        link: hex(0x2F4A6B),
        solid: hex(0x1D1C1A),
        solid_text: hex(0xF3F0E8),
    }
}

/// The palette for the theme's current mode.
pub fn palette(cx: &App) -> Palette {
    if cx.theme().mode.is_dark() {
        night()
    } else {
        day()
    }
}

/// The kit themes, in the kit's own theme-file format, so buttons,
/// inputs, popovers and the settings dialog follow the palette without
/// a per-widget override.
const THEMES: &str = r##"{
  "name": "Gertie",
  "author": "gertie",
  "themes": [
    {
      "name": "Gertie Night",
      "mode": "dark",
      "font.family": "Inter",
      "mono_font.family": "IBM Plex Mono",
      "radius": 8,
      "radius.lg": 14,
      "colors": {
        "background": "#23211E",
        "foreground": "#ECE7DC",
        "border": "#34312C",
        "accent.background": "#312E2A",
        "accent.foreground": "#ECE7DC",
        "muted.background": "#2A2824",
        "muted.foreground": "#A8A296",
        "secondary.background": "#2A2824",
        "secondary.hover.background": "#312E2A",
        "secondary.active.background": "#383530",
        "secondary.foreground": "#ECE7DC",
        "primary.background": "#ECE7DC",
        "primary.hover.background": "#F6F2EA",
        "primary.active.background": "#D9D3C7",
        "primary.foreground": "#1A1917",
        "popover.background": "#2A2824",
        "popover.foreground": "#ECE7DC",
        "input.border": "#4A463F",
        "ring": "#A8A296",
        "caret": "#ECE7DC",
        "selection.background": "#A9C4E844",
        "link": "#A9C4E8",
        "link.hover": "#ECE7DC",
        "link.active": "#ECE7DC",
        "danger.background": "#E0704F",
        "danger.foreground": "#1A1917",
        "warning.background": "#E0704F",
        "warning.foreground": "#1A1917",
        "success.background": "#A9C4E8",
        "success.foreground": "#1A1917",
        "info.background": "#A9C4E8",
        "info.foreground": "#1A1917",
        "list.background": "#23211E",
        "list.hover.background": "#2A2824",
        "list.active.background": "#ECE7DC14",
        "list.active.border": "#A8A296",
        "list.even.background": "#26241F",
        "list.head.background": "#2A2824",
        "list.highlight.foreground": "#A9C4E8",
        "sidebar.background": "#1A1917",
        "sidebar.foreground": "#ECE7DC",
        "sidebar.border": "#2C2A26",
        "sidebar.accent.background": "#2A2824",
        "sidebar.accent.foreground": "#ECE7DC",
        "sidebar.primary.background": "#ECE7DC",
        "sidebar.primary.foreground": "#1A1917",
        "switch.background": "#A9C4E8",
        "scrollbar.background": "#00000000",
        "scrollbar.thumb.background": "#5E5A5299",
        "scrollbar.thumb.hover.background": "#8E887C",
        "title_bar.background": "#1A1917",
        "title_bar.border": "#2C2A26",
        "tab_bar.background": "#1A1917",
        "overlay": "#0E0D0CB3",
        "base.blue": "#A9C4E8",
        "base.green": "#A9C4E8",
        "base.red": "#E0704F",
        "base.yellow": "#E0704F"
      }
    },
    {
      "name": "Gertie Day",
      "mode": "light",
      "font.family": "Inter",
      "mono_font.family": "IBM Plex Mono",
      "radius": 8,
      "radius.lg": 14,
      "colors": {
        "background": "#F3F0E8",
        "foreground": "#1D1C1A",
        "border": "#E2DDD1",
        "accent.background": "#EAE5DA",
        "accent.foreground": "#1D1C1A",
        "muted.background": "#EAE5DA",
        "muted.foreground": "#6B675E",
        "secondary.background": "#FBFAF6",
        "secondary.hover.background": "#EAE5DA",
        "secondary.active.background": "#E2DDD1",
        "secondary.foreground": "#1D1C1A",
        "primary.background": "#1D1C1A",
        "primary.hover.background": "#33312D",
        "primary.active.background": "#000000",
        "primary.foreground": "#F3F0E8",
        "popover.background": "#FBFAF6",
        "popover.foreground": "#1D1C1A",
        "input.border": "#CFC8B8",
        "ring": "#6B675E",
        "caret": "#1D1C1A",
        "selection.background": "#2F4A6B33",
        "link": "#2F4A6B",
        "link.hover": "#1D1C1A",
        "link.active": "#1D1C1A",
        "danger.background": "#B5432A",
        "danger.foreground": "#F3F0E8",
        "warning.background": "#B5432A",
        "warning.foreground": "#F3F0E8",
        "success.background": "#2F4A6B",
        "success.foreground": "#F3F0E8",
        "info.background": "#2F4A6B",
        "info.foreground": "#F3F0E8",
        "list.background": "#F3F0E8",
        "list.hover.background": "#EAE5DA",
        "list.active.background": "#1D1C1A10",
        "list.active.border": "#6B675E",
        "list.even.background": "#EFEBE2",
        "list.head.background": "#EAE5DA",
        "list.highlight.foreground": "#2F4A6B",
        "sidebar.background": "#F3F0E8",
        "sidebar.foreground": "#1D1C1A",
        "sidebar.border": "#E2DDD1",
        "sidebar.accent.background": "#EAE5DA",
        "sidebar.accent.foreground": "#1D1C1A",
        "sidebar.primary.background": "#1D1C1A",
        "sidebar.primary.foreground": "#F3F0E8",
        "switch.background": "#2F4A6B",
        "scrollbar.background": "#00000000",
        "scrollbar.thumb.background": "#A8A09299",
        "scrollbar.thumb.hover.background": "#6B675E",
        "title_bar.background": "#F3F0E8",
        "title_bar.border": "#E2DDD1",
        "tab_bar.background": "#F3F0E8",
        "overlay": "#1D1C1A66",
        "base.blue": "#2F4A6B",
        "base.green": "#2F4A6B",
        "base.red": "#B5432A",
        "base.yellow": "#B5432A"
      }
    }
  ]
}"##;

/// The embedded faces, subset to Latin plus the punctuation, arrows and
/// marks the UI draws (`assets/fonts`, OFL, licenses beside them).
fn fonts() -> Vec<std::borrow::Cow<'static, [u8]>> {
    vec![
        include_bytes!("../../assets/fonts/Inter-Regular.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/Inter-Medium.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/ShipporiMincho-Regular.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/ShipporiMincho-SemiBold.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf")
            .as_slice()
            .into(),
        include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf")
            .as_slice()
            .into(),
    ]
}

/// Loads the faces and installs Night and Day as the kit's themes for
/// their modes, then applies `mode`. Every later `Theme::change` (the
/// settings toggle) reloads these, not the kit's defaults.
pub fn install(mode: ThemeMode, cx: &mut App) {
    // The motion switch (Chat settings): gpui's own reduce-motion
    // flag, which every animation already respects.
    cx.set_reduce_motion(!crate::settings::load_motion());
    if let Err(error) = cx.text_system().add_fonts(fonts()) {
        eprintln!("gertie: could not load the embedded fonts: {error}");
    }
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(error) = registry.load_themes_from_str(THEMES) {
        eprintln!("gertie: could not load the gertie themes: {error}");
    }
    let themes = ThemeRegistry::global(cx).themes().clone();
    let night = themes.get(&SharedString::from("Gertie Night")).cloned();
    let day = themes.get(&SharedString::from("Gertie Day")).cloned();
    // The first change installs the global theme when it isn't yet;
    // the swap below then lands on a theme that exists.
    Theme::change(mode, None, cx);
    let theme = Theme::global_mut(cx);
    if let Some(night) = night {
        theme.dark_theme = Rc::clone(&night);
    }
    if let Some(day) = day {
        theme.light_theme = Rc::clone(&day);
    }
    Theme::change(mode, None, cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_theme_file_parses() {
        let value: serde_json::Value = serde_json::from_str(super::THEMES).unwrap();
        assert_eq!(value["themes"].as_array().unwrap().len(), 2);
    }
}

//! Colours and widget look for the two app modes, following the Windows "Choose your app mode" setting.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Theme, Visuals};

/// The colours of one app mode. Extension swatches are picked by rank, so colours stay with their extension.
pub struct Palette {
    pub bg: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub header: Color32,
    pub rule: Color32,
    /// Borders between panes, and under table headers.
    pub separator: Color32,
    /// Table header rows, a shade off the pane background.
    pub header_bg: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub selected: Color32,
    pub error: Color32,
    /// Extension colours for the first seven ranks; later ranks and unknown items share `neutral`.
    pub swatches: [Color32; 7],
    pub neutral: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

const LIGHT: Palette = Palette {
    bg: hex(0xfffaf7),
    text: hex(0x4a3f44),
    muted: hex(0xa8929a),
    header: hex(0xb9a3a8),
    rule: hex(0xf4e7e2),
    separator: hex(0xe3cfc8),
    header_bg: hex(0xf9f0eb),
    accent: hex(0xff8fa6),
    accent_hover: hex(0xff7692),
    on_accent: hex(0xffffff),
    selected: hex(0xffe9ee),
    error: hex(0xc8283c),
    swatches: [hex(0xff8fa6), hex(0x9fd0ec), hex(0xb5dfa8), hex(0xfbd685), hex(0xcbb4ee), hex(0xffb89c), hex(0xbcd2e0)],
    neutral: hex(0xe8d9d4),
};

const DARK: Palette = Palette {
    bg: hex(0x1d1619),
    text: hex(0xefe3e7),
    muted: hex(0x9d868f),
    header: hex(0x8f7882),
    rule: hex(0x2c2226),
    separator: hex(0x3a2c33),
    header_bg: hex(0x251c21),
    accent: hex(0xff8fa6),
    accent_hover: hex(0xffa6b8),
    on_accent: hex(0x3f1220),
    selected: hex(0x3a2530),
    error: hex(0xff7a8a),
    swatches: [hex(0xff8fa6), hex(0x7fb9dc), hex(0x9fcf92), hex(0xe9c370), hex(0xb79fdf), hex(0xf0a488), hex(0x6d8799)],
    neutral: hex(0x7d6a72),
};

pub fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// Colour of the extension at `rank` (its position in `Tree::exts`).
pub fn swatch(dark: bool, rank: u16) -> Color32 {
    let p = palette(dark);
    p.swatches.get(rank as usize).copied().unwrap_or(p.neutral)
}

/// Dark when the registry's `AppsUseLightTheme` is 0; light when it is 1 or missing (as Windows then shows light apps).
pub fn dark_from_registry(apps_use_light_theme: Option<u32>) -> bool {
    apps_use_light_theme == Some(0)
}

/// Reads the Windows app mode from the registry, as the shell menu does.
pub fn system_dark() -> bool {
    dark_from_registry(crate::actions::apps_use_light_theme())
}

/// Sets both looks, then shows the one the Windows app mode asks for. Call at startup and when the mode may have changed.
pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Light, visuals(false));
    ctx.set_visuals_of(Theme::Dark, visuals(true));
    ctx.set_theme(if system_dark() { Theme::Dark } else { Theme::Light });
}

fn visuals(dark: bool) -> Visuals {
    let p = palette(dark);
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.faint_bg_color = p.bg;
    v.extreme_bg_color = p.rule;
    v.selection.bg_fill = p.accent;
    v.error_fg_color = p.error;
    v.window_stroke = Stroke::new(1.0, p.rule);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::ZERO;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke.color = p.text;
    }
    v.widgets.inactive.weak_bg_fill = p.bg;
    v.widgets.hovered.weak_bg_fill = p.selected;
    v.widgets.active.weak_bg_fill = p.selected;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_value_zero_is_dark_and_anything_else_is_light() {
        assert!(dark_from_registry(Some(0)));
        assert!(!dark_from_registry(Some(1)));
        assert!(!dark_from_registry(None));
    }

    #[test]
    fn palette_follows_the_theme() {
        assert_ne!(palette(true).bg, palette(false).bg);
        assert_eq!(palette(true).accent, palette(false).accent, "the accent pink is shared");
        assert_eq!(palette(false).bg, Color32::from_rgb(0xff, 0xfa, 0xf7));
        assert_eq!(palette(true).bg, Color32::from_rgb(0x1d, 0x16, 0x19));
    }

    #[test]
    fn swatches_are_stable_distinct_and_neutral_past_the_palette() {
        for dark in [false, true] {
            let p = palette(dark);
            let chromatic: std::collections::HashSet<Color32> = p.swatches.iter().copied().collect();
            assert_eq!(chromatic.len(), 7);
            assert!(!chromatic.contains(&p.neutral));
            assert_eq!(swatch(dark, 0), p.swatches[0]);
            assert_eq!(swatch(dark, 7), p.neutral);
            assert_eq!(swatch(dark, u16::MAX), p.neutral);
        }
    }

    #[test]
    fn every_widget_and_window_is_square() {
        for dark in [false, true] {
            let v = visuals(dark);
            for w in [v.widgets.noninteractive, v.widgets.inactive, v.widgets.hovered, v.widgets.active, v.widgets.open] {
                assert_eq!(w.corner_radius, CornerRadius::ZERO);
            }
            assert_eq!(v.window_corner_radius, CornerRadius::ZERO);
            assert_eq!(v.menu_corner_radius, CornerRadius::ZERO);
        }
    }
}

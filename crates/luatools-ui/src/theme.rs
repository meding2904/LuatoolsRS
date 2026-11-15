//! Design tokens and global style installation.
//!
//! Every colour, radius, and spacing value used by the interface comes from [`Theme`], so a
//! palette change never requires touching a page.

use eframe::egui::{
    self, Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals,
};

/// Dark interface tokens.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Window background behind cards.
    pub bg: Color32,
    /// Side and top bar background.
    pub panel: Color32,
    /// Card and dialog background.
    pub surface: Color32,
    /// Hovered surface.
    pub surface_hover: Color32,
    /// Hairline borders.
    pub border: Color32,
    /// Stronger borders, e.g. inputs on focus.
    pub border_strong: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text and labels.
    pub muted: Color32,
    /// Disabled text.
    pub faint: Color32,
    /// Brand accent, used for primary actions and selection.
    pub accent: Color32,
    /// Accent on hover.
    pub accent_hover: Color32,
    /// Text drawn on top of the accent.
    pub on_accent: Color32,
    /// Destructive actions and errors.
    pub danger: Color32,
    /// Success states.
    pub success: Color32,
    /// Warning states.
    pub warning: Color32,
    /// Scrim behind modals.
    pub scrim: Color32,
    /// Body text size.
    pub text_size: f32,
    /// Small label size.
    pub small_size: f32,
    /// Page title size.
    pub title_size: f32,
    /// Card corner radius.
    pub radius_card: CornerRadius,
    /// Control corner radius.
    pub radius_control: CornerRadius,
    /// Pill corner radius.
    pub radius_pill: CornerRadius,
    /// Standard gap.
    pub gap: f32,
    /// Page padding.
    pub page_padding: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// The dark palette.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            // Original LuaTools palette: near-black window, 5% white cards, violet brand accent.
            bg: Color32::from_rgb(22, 22, 28),
            panel: Color32::from_rgb(27, 27, 33),
            surface: Color32::from_rgb(34, 34, 42),
            surface_hover: Color32::from_rgb(44, 44, 54),
            border: Color32::from_rgb(51, 51, 61),
            border_strong: Color32::from_rgb(68, 68, 80),
            text: Color32::from_rgb(255, 255, 255),
            muted: Color32::from_rgb(156, 163, 175),
            faint: Color32::from_rgb(107, 114, 128),
            accent: Color32::from_rgb(167, 139, 250),
            accent_hover: Color32::from_rgb(196, 181, 253),
            on_accent: Color32::from_rgb(20, 20, 28),
            danger: Color32::from_rgb(248, 113, 113),
            success: Color32::from_rgb(74, 222, 128),
            warning: Color32::from_rgb(251, 191, 36),
            scrim: Color32::from_black_alpha(153),
            text_size: 13.0,
            small_size: 11.5,
            title_size: 22.0,
            radius_card: CornerRadius::same(10),
            radius_control: CornerRadius::same(6),
            radius_pill: CornerRadius::same(255),
            gap: 8.0,
            page_padding: 16.0,
        }
    }

    /// The light palette.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            bg: Color32::from_rgb(247, 248, 250),
            panel: Color32::from_rgb(255, 255, 255),
            surface: Color32::from_rgb(255, 255, 255),
            surface_hover: Color32::from_rgb(240, 242, 246),
            border: Color32::from_rgb(224, 227, 233),
            border_strong: Color32::from_rgb(198, 204, 214),
            text: Color32::from_rgb(21, 24, 30),
            muted: Color32::from_rgb(96, 104, 118),
            faint: Color32::from_rgb(140, 148, 161),
            accent: Color32::from_rgb(20, 106, 214),
            accent_hover: Color32::from_rgb(12, 88, 186),
            on_accent: Color32::from_rgb(255, 255, 255),
            danger: Color32::from_rgb(196, 46, 46),
            success: Color32::from_rgb(22, 138, 84),
            warning: Color32::from_rgb(166, 112, 0),
            scrim: Color32::from_black_alpha(90),
            ..Self::dark()
        }
    }

    /// Installs colours, spacing, and text styles on the context.
    pub fn install(&self, ctx: &Context, dark_mode: bool) {
        install_fonts(ctx);
        ctx.set_theme(if dark_mode {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });

        let mut visuals = if dark_mode {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        visuals.panel_fill = self.bg;
        visuals.window_fill = self.surface;
        visuals.extreme_bg_color = self.bg;
        visuals.faint_bg_color = self.surface;
        visuals.code_bg_color = self.surface;
        visuals.override_text_color = Some(self.text);
        visuals.window_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.noninteractive.bg_fill = self.surface;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        visuals.widgets.inactive.bg_fill = self.surface;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, self.text);
        visuals.widgets.hovered.bg_fill = self.surface_hover;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, self.border_strong);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, self.text);
        visuals.widgets.active.bg_fill = self.surface_hover;
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, self.accent);
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, self.text);
        visuals.widgets.open.bg_fill = self.surface;
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, self.border_strong);
        visuals.widgets.open.fg_stroke = Stroke::new(1.0, self.text);
        visuals.selection.bg_fill = self.accent.gamma_multiply(0.28);
        visuals.selection.stroke = Stroke::new(1.0, self.accent);
        visuals.hyperlink_color = self.accent;
        visuals.warn_fg_color = self.warning;
        visuals.error_fg_color = self.danger;
        ctx.set_visuals_of(
            if dark_mode {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            },
            visuals,
        );

        let theme = if dark_mode {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };
        let mut style = (*ctx.style_of(theme)).clone();
        style.spacing.item_spacing = egui::vec2(self.gap, self.gap);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = Margin::same(12);
        style.spacing.menu_margin = Margin::same(8);
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(self.title_size, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                FontId::new(self.text_size, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                FontId::new(self.text_size, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(self.small_size, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(self.small_size + 1.0, FontFamily::Monospace),
            ),
        ]
        .into();
        ctx.set_style_of(theme, style);
    }

    /// A frame for a card.
    pub fn card_frame(self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.surface)
            .stroke(Stroke::new(1.0, self.border))
            .corner_radius(self.radius_card)
            .inner_margin(Margin::same(16))
    }

    /// Returns the colour for a semantic tone.
    #[must_use]
    pub fn tone(self, tone: Tone) -> Color32 {
        match tone {
            Tone::Accent => self.accent,
            Tone::Success => self.success,
            Tone::Warning => self.warning,
            Tone::Danger => self.danger,
            Tone::Muted => self.muted,
            Tone::Text => self.text,
        }
    }
}

/// Installs system fallback fonts once per process.
///
/// egui's bundled fonts cover Latin, Greek, Cyrillic and emoji but not CJK or Arabic, so the
/// 29 shipped locales would render as boxes without these. Files are loaded from well-known
/// OS locations; a missing font is not an error, the text simply falls back to the default.
pub fn install_fonts(ctx: &Context) {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static INSTALLED: OnceLock<()> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);

    INSTALLED.get_or_init(|| {
        for (name, candidates) in [
            (
                "system-cjk",
                &[
                    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                    "C:/Windows/Fonts/msyh.ttc",
                    "C:/Windows/Fonts/simsun.ttc",
                    "/System/Library/Fonts/PingFang.ttc",
                ][..],
            ),
            (
                "system-thai",
                &[
                    "/usr/share/fonts/noto/NotoSansThai-Regular.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
                    "C:/Windows/Fonts/leelawui.ttf",
                    "/System/Library/Fonts/Thonburi.ttc",
                ][..],
            ),
            (
                "system-arabic",
                &[
                    "/usr/share/fonts/noto/NotoKufiArabic-Regular.ttf",
                    "/usr/share/fonts/noto/NotoSansArabicUI-Regular.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf",
                    "C:/Windows/Fonts/segoeui.ttf",
                    "/System/Library/Fonts/GeezaPro.ttc",
                ][..],
            ),
        ] {
            let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) else {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::debug!(name, "no system fallback font found");
                }
                continue;
            };
            ctx.add_font(egui::FontInsert::new(
                name,
                egui::FontData::from_owned(bytes),
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
        }
    });
}

/// Semantic colours used by widgets and status rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Brand emphasis.
    Accent,
    /// Positive outcome.
    Success,
    /// Needs attention.
    Warning,
    /// Failure or destructive action.
    Danger,
    /// Secondary information.
    Muted,
    /// Primary text.
    Text,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_differ_in_background_and_text() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_ne!(dark.bg, light.bg);
        assert!(dark.text.r() > 200 && light.text.r() < 60);
    }

    #[test]
    fn every_tone_maps_to_a_distinct_colour() {
        let theme = Theme::dark();
        let tones = [
            Tone::Accent,
            Tone::Success,
            Tone::Warning,
            Tone::Danger,
            Tone::Muted,
            Tone::Text,
        ];
        let colours: std::collections::HashSet<_> = tones.iter().map(|t| theme.tone(*t)).collect();
        assert_eq!(colours.len(), tones.len());
    }
}

//! Reusable controls shared by every page.
//!
//! Widgets take [`Theme`] explicitly instead of reaching for global state, which keeps them
//! testable and lets the settings preview render with a different palette.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Button, Color32, CornerRadius, Layout, Margin, Response, RichText, Sense, Stroke,
    Ui,
};

use crate::theme::{Theme, Tone};

/// Maximum lines kept in the toast stack.
const MAX_TOASTS: usize = 4;

/// How long a toast stays on screen.
const TOAST_LIFETIME: Duration = Duration::from_secs(6);

/// Draws a card with the standard padding and border.
pub fn card(ui: &mut Ui, theme: &Theme, add_contents: impl FnOnce(&mut Ui)) {
    theme.card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui);
    });
}

/// Draws a card whose whole surface is clickable.
///
/// Only use this for rows without interactive children: the returned interaction sits above
/// the content so that a click anywhere selects the row.
pub fn clickable_card(
    ui: &mut Ui,
    theme: &Theme,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    add_contents: impl FnOnce(&mut Ui),
) -> Response {
    let inner = ui.scope(|ui| card(ui, theme, add_contents));
    ui.interact(
        inner.response.rect,
        ui.scope_id().with(id_salt),
        Sense::click(),
    )
}

/// Draws a horizontal hairline.
pub fn divider(ui: &mut Ui, theme: &Theme) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, theme.border);
}

/// A page title with an optional supporting line.
pub fn page_header(ui: &mut Ui, theme: &Theme, title: &str, subtitle: Option<&str>) {
    ui.label(
        RichText::new(title)
            .size(theme.title_size)
            .color(theme.text),
    );
    if let Some(subtitle) = subtitle {
        ui.add_space(2.0);
        ui.label(
            RichText::new(subtitle)
                .size(theme.small_size + 0.5)
                .color(theme.muted),
        );
    }
}

/// A small muted section label.
pub fn section_label(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .size(theme.small_size)
            .color(theme.muted)
            .strong(),
    );
}

/// A filled primary action.
pub fn primary_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        Button::new(
            RichText::new(text)
                .color(theme.on_accent)
                .size(theme.text_size),
        )
        .fill(theme.accent)
        .corner_radius(theme.radius_control)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// An outlined secondary action.
pub fn secondary_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    let text_color = if ui.is_enabled() {
        theme.text
    } else {
        theme.faint
    };
    ui.add(
        Button::new(RichText::new(text).color(text_color).size(theme.text_size))
            .fill(Color32::TRANSPARENT)
            .corner_radius(theme.radius_control)
            .stroke(Stroke::new(1.0, theme.border_strong))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// A destructive action.
pub fn danger_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        Button::new(
            RichText::new(text)
                .color(theme.danger)
                .size(theme.text_size),
        )
        .fill(Color32::TRANSPARENT)
        .corner_radius(theme.radius_control)
        .stroke(Stroke::new(1.0, theme.danger.gamma_multiply(0.5)))
        .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// A borderless text action.
pub fn link_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        Button::new(
            RichText::new(text)
                .color(theme.accent)
                .size(theme.text_size),
        )
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::NONE)
        .corner_radius(theme.radius_control),
    )
}

/// One row in the left navigation rail.
pub fn nav_item(ui: &mut Ui, theme: &Theme, label: &str, selected: bool) -> Response {
    let fill = if selected {
        theme.surface
    } else {
        Color32::TRANSPARENT
    };
    let text_color = if selected { theme.text } else { theme.muted };
    let response = ui.add_sized(
        [ui.available_width(), 34.0],
        Button::new(RichText::new(label).color(text_color).size(theme.text_size))
            .fill(fill)
            .corner_radius(theme.radius_control)
            .stroke(Stroke::NONE),
    );
    if selected {
        let rect = response.rect;
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 8.0),
            egui::vec2(2.5, rect.height() - 16.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), theme.accent);
    }
    response
}

/// A toggleable filter chip.
pub fn chip(ui: &mut Ui, theme: &Theme, label: &str, selected: bool) -> Response {
    let (fill, text, stroke) = if selected {
        (
            theme.accent.gamma_multiply(0.18),
            theme.accent,
            Stroke::new(1.0, theme.accent.gamma_multiply(0.55)),
        )
    } else {
        (
            Color32::TRANSPARENT,
            theme.muted,
            Stroke::new(1.0, theme.border),
        )
    };
    ui.add(
        Button::new(
            RichText::new(label)
                .size(theme.small_size + 0.5)
                .color(text),
        )
        .fill(fill)
        .corner_radius(theme.radius_pill)
        .stroke(stroke),
    )
}

/// A small coloured badge, e.g. `Installed`.
pub fn badge(ui: &mut Ui, theme: &Theme, text: &str, tone: Tone) {
    let colour = theme.tone(tone);
    egui::Frame::new()
        .fill(colour.gamma_multiply(0.16))
        .corner_radius(theme.radius_pill)
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(theme.small_size).color(colour));
        });
}

/// A label/value pair used in the stats strip.
pub fn stat(ui: &mut Ui, theme: &Theme, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(value)
                .size(theme.title_size - 4.0)
                .color(theme.text)
                .strong(),
        );
        ui.label(
            RichText::new(label)
                .size(theme.small_size)
                .color(theme.muted),
        );
    });
}

/// A search input with a subtle background.
pub fn search_field(ui: &mut Ui, theme: &Theme, value: &mut String, hint: &str) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(hint).color(theme.faint))
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 7)),
    )
}

/// A progress bar with an optional indeterminate state.
pub fn progress_bar(ui: &mut Ui, theme: &Theme, fraction: Option<f32>) {
    let height = 6.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), theme.border);
    let filled = match fraction {
        Some(f) => {
            let width = rect.width() * f.clamp(0.0, 1.0);
            egui::Rect::from_min_size(rect.min, egui::vec2(width, height))
        }
        None => {
            // Indeterminate: a short segment anchored ahead of the midpoint.
            let width = rect.width() * 0.25;
            let offset = rect.width() * 0.375;
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + offset, rect.top()),
                egui::vec2(width, height),
            )
        }
    };
    ui.painter()
        .rect_filled(filled, CornerRadius::same(3), theme.accent);
}

/// An empty-list placeholder.
pub fn empty_state(ui: &mut Ui, theme: &Theme, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.label(
            RichText::new(title)
                .size(theme.text_size + 2.0)
                .color(theme.text),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(body)
                .size(theme.small_size + 0.5)
                .color(theme.muted),
        );
        ui.add_space(16.0);
    });
}

/// A key/value row used in detail panes.
pub fn field_row(ui: &mut Ui, theme: &Theme, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [150.0, 18.0],
            egui::Label::new(
                RichText::new(label)
                    .size(theme.small_size + 0.5)
                    .color(theme.muted),
            ),
        );
        ui.label(
            RichText::new(value)
                .size(theme.small_size + 0.5)
                .color(theme.text),
        );
    });
}

/// Renders the toast stack in the bottom-right corner and drops expired entries.
pub fn toast_stack(ui: &mut Ui, theme: &Theme, toasts: &mut Vec<crate::state::ToastItem>) {
    let ctx = ui.ctx().clone();
    let now = Instant::now();
    toasts.retain(|item| now.duration_since(item.shown_at) < TOAST_LIFETIME);
    if toasts.is_empty() {
        return;
    }
    let anchor = egui::Align2::RIGHT_BOTTOM;
    egui::Area::new(egui::Id::unique("toast-stack"))
        .anchor(anchor, egui::vec2(-20.0, -20.0))
        .order(egui::Order::Foreground)
        .interactable(true)
        .show(&ctx, |ui| {
            ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                for item in toasts.iter().rev().take(MAX_TOASTS) {
                    let colour = match item.kind {
                        crate::state::ToastKind::Info => theme.accent,
                        crate::state::ToastKind::Success => theme.success,
                        crate::state::ToastKind::Warning => theme.warning,
                        crate::state::ToastKind::Error => theme.danger,
                    };
                    egui::Frame::new()
                        .fill(theme.surface)
                        .stroke(Stroke::new(1.0, colour.gamma_multiply(0.6)))
                        .corner_radius(theme.radius_control)
                        .inner_margin(Margin::symmetric(12, 9))
                        .show(ui, |ui| {
                            ui.set_max_width(360.0);
                            ui.label(
                                RichText::new(&item.message)
                                    .size(theme.small_size + 0.5)
                                    .color(theme.text),
                            );
                        });
                    ui.add_space(6.0);
                }
            });
        });
    ctx.request_repaint_after(Duration::from_millis(500));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_constants_are_sane() {
        assert!(TOAST_LIFETIME.as_secs() >= 3);
        assert_eq!(MAX_TOASTS, 4);
    }

    #[test]
    fn widgets_render_in_a_headless_context() {
        let ctx = egui::Context::default();
        let theme = Theme::dark();
        let mut text = String::new();
        let mut output = ctx.run_ui(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                page_header(ui, &theme, "Library", Some("3 apps"));
                card(ui, &theme, |ui| {
                    if primary_button(ui, &theme, "Download").clicked() {
                        text.push('d');
                    }
                    secondary_button(ui, &theme, "Cancel");
                    badge(ui, &theme, "Installed", Tone::Success);
                    search_field(ui, &theme, &mut text, "Search");
                    progress_bar(ui, &theme, Some(0.5));
                    progress_bar(ui, &theme, None);
                    stat(ui, &theme, "Installed", "12");
                    chip(ui, &theme, "RPG", true);
                });
                empty_state(ui, &theme, "Nothing here", "Add a download to begin");
                divider(ui, &theme);
            });
        });
        // No renderer consumes the texture delta in a headless pass.
        output.textures_delta.clear();
    }
}

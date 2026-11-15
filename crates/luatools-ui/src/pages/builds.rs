//! Library browser: search, filter, and select an app.

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Label, Layout, RichText, Sense, Stroke, StrokeKind,
};
use luatools_core::{AppId, ByteFormat};
use luatools_i18n::Key;

use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// Builds page transient state.
#[derive(Debug, Default)]
pub struct State {
    /// Show only installed apps.
    pub installed_only: bool,
}

/// Renders the library browser.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let search_hint = app.t(Key::Common_SearchPlaceholder);
    let clear_label = app.t(Key::Manage_ClearFilters);
    let no_match = app.t(Key::Manage_Empty_NoMatch);

    widgets::page_header(
        ui,
        &theme,
        app.t(luatools_i18n::Key::Nav_Builds),
        Some("Installed and owned apps across every Steam library."),
    );
    ui.add_space(16.0);

    ui.horizontal(|ui| {
        let chip_width = 128.0;
        let search_width = (ui.available_width() - chip_width - theme.gap).max(200.0);
        ui.allocate_ui_with_layout(
            egui::vec2(search_width, 30.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                widgets::search_field(ui, &theme, &mut app.search, search_hint);
            },
        );
        if widgets::chip(ui, &theme, "Installed only", state.installed_only).clicked() {
            state.installed_only = !state.installed_only;
        }
    });
    ui.add_space(10.0);

    let ids: Vec<AppId> = app
        .snapshot
        .filtered(&app.search, state.installed_only)
        .map(|entry| entry.id)
        .collect();
    let total = app.snapshot.apps.len();
    let filtering = !app.search.is_empty() || state.installed_only;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(result_count(ids.len(), total))
                .size(theme.small_size + 0.5)
                .color(theme.muted),
        );
        if filtering {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::link_button(ui, &theme, clear_label).clicked() {
                    app.search.clear();
                    state.installed_only = false;
                }
            });
        }
    });
    ui.add_space(8.0);

    if ids.is_empty() {
        if app.snapshot.apps.is_empty() {
            widgets::empty_state(
                ui,
                &theme,
                "No library yet",
                "Refresh the library from Home to scan the Steam folders on this computer.",
            );
        } else {
            widgets::empty_state(
                ui,
                &theme,
                no_match,
                "Try a different search, or turn off the installed-only filter.",
            );
        }
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("builds-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for id in ids {
                let Some((name, installed, size)) = app.snapshot.app(id).map(|entry| {
                    (
                        entry.name.clone(),
                        entry.installed.is_installed(),
                        entry.installed.size_on_disk(),
                    )
                }) else {
                    continue;
                };
                let selected = app.pages.manage.selected == Some(id);

                let response = widgets::clickable_card(ui, &theme, id, |ui| {
                    ui.horizontal(|ui| {
                        cover_thumb(ui, &theme, &name);
                        let meta_width = 230.0;
                        let name_width = (ui.available_width() - meta_width).max(140.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(name_width, 40.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.add(
                                    Label::new(
                                        RichText::new(&name)
                                            .size(theme.text_size + 0.5)
                                            .color(theme.text)
                                            .strong(),
                                    )
                                    .truncate(),
                                );
                                ui.label(
                                    RichText::new(format!("App {id}"))
                                        .size(theme.small_size)
                                        .color(theme.muted),
                                );
                            },
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if installed {
                                widgets::badge(ui, &theme, "Installed", Tone::Success);
                            } else {
                                widgets::badge(ui, &theme, "Not installed", Tone::Muted);
                            }
                            if installed && size > 0 {
                                ui.label(
                                    RichText::new(ByteFormat::new(size).to_string())
                                        .size(theme.small_size + 0.5)
                                        .color(theme.muted),
                                );
                            }
                        });
                    });
                });

                if selected {
                    ui.painter().rect_stroke(
                        response.rect,
                        theme.radius_card,
                        Stroke::new(1.5, theme.accent),
                        StrokeKind::Inside,
                    );
                } else if response.hovered() {
                    ui.painter().rect_stroke(
                        response.rect,
                        theme.radius_card,
                        Stroke::new(1.0, theme.border_strong),
                        StrokeKind::Inside,
                    );
                }
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() {
                    app.pages.manage.selected = Some(id);
                    app.ensure_details(id);
                }
                ui.add_space(8.0);
            }
        });
}

/// "12 apps" or "3 of 25 apps".
fn result_count(shown: usize, total: usize) -> String {
    let noun = if shown == 1 { "app" } else { "apps" };
    if shown == total {
        format!("{shown} {noun}")
    } else {
        format!("{shown} of {total} apps")
    }
}

/// A small rounded cover stand-in with the app's initial.
fn cover_thumb(ui: &mut egui::Ui, theme: &Theme, name: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(44.0, 28.0), Sense::hover());
    let radius = CornerRadius::same(5);
    ui.painter().rect_filled(rect, radius, theme.bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, theme.border),
        StrokeKind::Inside,
    );
    let initial = name
        .chars()
        .find(|c| !c.is_whitespace())
        .map_or('?', |c| c.to_ascii_uppercase());
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::proportional(14.0),
        theme.accent,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use luatools_core::{AppEntry, BackendHandle, InstalledState, Settings};

    use super::*;

    fn state() -> AppState {
        AppState::new(BackendHandle::disconnected(), Settings::default())
    }

    fn render(state: &mut AppState, page_state: &mut State) {
        let ctx = egui::Context::default();
        state.theme.install(&ctx, state.dark_mode);
        let mut output = ctx.run_ui(Default::default(), |ui| {
            show(ui, state, page_state);
        });
        // No renderer consumes the texture delta in a headless pass.
        output.textures_delta.clear();
    }

    #[test]
    fn renders_searchable_result_rows() {
        let mut app = state();
        let mut cs2 = AppEntry::new(AppId::new(730), "Counter-Strike 2");
        cs2.installed = InstalledState::Installed {
            path: PathBuf::from("/steam/steamapps/common/Counter-Strike 2"),
            size_on_disk: 30_000_000_000,
            build_id: Some("17800000".to_owned()),
        };
        app.snapshot.apps = vec![cs2, AppEntry::new(AppId::new(400), "Portal")];
        app.search = "counter".to_owned();
        app.pages.manage.selected = Some(AppId::new(730));

        let mut page_state = State::default();
        render(&mut app, &mut page_state);
    }

    #[test]
    fn renders_empty_filtered_state() {
        let mut app = state();
        app.snapshot.apps = vec![AppEntry::new(AppId::new(400), "Portal")];
        let mut page_state = State {
            installed_only: true,
        };
        render(&mut app, &mut page_state);
        assert!(app.snapshot.filtered(&app.search, true).next().is_none());
    }

    #[test]
    fn result_counts_read_naturally() {
        assert_eq!(result_count(1, 1), "1 app");
        assert_eq!(result_count(12, 12), "12 apps");
        assert_eq!(result_count(3, 25), "3 of 25 apps");
    }
}

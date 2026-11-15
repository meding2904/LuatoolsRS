//! Settings.

use eframe::egui::{self, Align, Label, Layout, RichText};
use std::path::PathBuf;

use luatools_core::{Command, DownloadScheduling, Locale, Settings, ThemePreference};
use luatools_i18n::Key;

use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// Settings page transient state.
#[derive(Debug, Default)]
pub struct State {
    /// Editable copy of the settings, created on first render.
    pub draft: Option<Settings>,
    /// Whether the draft differs from what is active.
    pub dirty: bool,
    /// Pending native folder picker for the download directory.
    pub download_picker: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    /// Pending native folder picker for the Steam root.
    pub steam_picker: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
}

impl State {
    /// Loads the draft from the active settings when first needed.
    pub fn draft_mut(&mut self, current: &Settings) -> &mut Settings {
        self.draft.get_or_insert_with(|| current.clone())
    }
}

/// Whether saving would do anything. Kept pure so the button state is testable.
fn save_enabled(draft: Option<&Settings>, current: &Settings) -> bool {
    draft.is_some_and(|draft| draft != current)
}

/// Renders settings.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let current = app.settings.clone();
    widgets::page_header(
        ui,
        &theme,
        app.t(Key::Settings_Title),
        Some("Language, download behaviour, and network options."),
    );
    ui.add_space(12.0);

    let mut save = false;
    let mut reset = false;
    let mut pick_download = false;
    let mut pick_steam = false;

    if let Some(result) = crate::dialogs::poll(&mut state.download_picker)
        && let Some(path) = result
    {
        state.draft_mut(&current).download_dir = path;
    }
    if let Some(result) = crate::dialogs::poll(&mut state.steam_picker)
        && let Some(path) = result
    {
        state.draft_mut(&current).steam_path = Some(path);
    }

    // Leave room for the action bar below the scroll area.
    let scroll_height = (ui.available_height() - 80.0).max(160.0);
    egui::ScrollArea::vertical()
        .max_height(scroll_height)
        .show(ui, |ui| {
            let draft = state.draft_mut(&current);

            section(ui, &theme, "GENERAL", |ui| {
                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Language");
                    let combo = egui::ComboBox::from_id_salt("settings-language")
                        .selected_text(draft.language.native_name())
                        .show_ui(ui, |ui| {
                            for locale in Locale::ALL {
                                ui.selectable_value(
                                    &mut draft.language,
                                    *locale,
                                    locale.native_name(),
                                );
                            }
                        });
                    label_widget(ui, combo.response.id, "Language");
                });
                hint(ui, &theme, "Applies to every screen.");
                row_divider(ui, &theme);

                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Theme");
                    for (preference, label) in [
                        (ThemePreference::System, "System"),
                        (ThemePreference::Dark, "Dark"),
                        (ThemePreference::Light, "Light"),
                    ] {
                        ui.selectable_value(&mut draft.theme, preference, label);
                    }
                });
                row_divider(ui, &theme);

                ui.checkbox(&mut draft.start_minimized, "Start minimized in the tray");
                ui.checkbox(&mut draft.check_updates, "Check for updates on launch");
                ui.checkbox(&mut draft.analytics_enabled, "Share anonymous analytics");
            });

            section(ui, &theme, "DOWNLOADS", |ui| {
                widgets::field_row(
                    ui,
                    &theme,
                    "Folder",
                    &draft.download_dir.display().to_string(),
                );
                ui.horizontal(|ui| {
                    ui.add_space(150.0 + theme.gap);
                    if widgets::secondary_button(ui, &theme, "Choose…").clicked() {
                        pick_download = true;
                    }
                    hint(ui, &theme, "New downloads are written here.");
                });
                row_divider(ui, &theme);

                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Scheduling");
                    ui.selectable_value(
                        &mut draft.scheduling,
                        DownloadScheduling::Sequential,
                        "Sequential",
                    );
                    ui.selectable_value(
                        &mut draft.scheduling,
                        DownloadScheduling::Parallel,
                        "Parallel",
                    );
                });
                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Concurrency");
                    let enabled = draft.scheduling == DownloadScheduling::Parallel;
                    ui.add_enabled_ui(enabled, |ui| {
                        ui.add(
                            egui::DragValue::new(&mut draft.max_concurrent_downloads)
                                .range(1..=16)
                                .suffix(" jobs"),
                        );
                    });
                });
                hint(ui, &theme, "Only used when downloads run in parallel.");
                row_divider(ui, &theme);

                ui.checkbox(
                    &mut draft.verify_after_download,
                    "Verify files after download",
                );
            });

            section(ui, &theme, "ADVANCED", |ui| {
                ui.checkbox(&mut draft.use_doh, "Resolve DNS over HTTPS");
                row_divider(ui, &theme);

                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Proxy");
                    let mut proxy = draft.proxy_url.clone().unwrap_or_default();
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut proxy)
                            .hint_text("http://host:port")
                            .desired_width(260.0),
                    );
                    if response.changed() {
                        draft.proxy_url = Some(proxy).filter(|value| !value.trim().is_empty());
                    }
                });
                hint(ui, &theme, "Leave blank to use the system proxy.");
                row_divider(ui, &theme);

                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Local API port");
                    let drag = ui.add(
                        egui::DragValue::new(&mut draft.local_api_port)
                            .range(0..=65535)
                            .speed(1.0),
                    );
                    label_widget(ui, drag.id, "Local API port");
                });
                hint(ui, &theme, "0 picks a free port.");
                row_divider(ui, &theme);

                let detected = app.snapshot.steam_root.as_ref().map_or_else(
                    || "not detected".to_owned(),
                    |path| path.display().to_string(),
                );
                let steam_path = draft
                    .steam_path
                    .as_ref()
                    .map_or_else(|| detected.clone(), |path| path.display().to_string());
                ui.horizontal(|ui| {
                    field_label(ui, &theme, "Steam folder");
                    ui.label(egui::RichText::new(steam_path).color(theme.muted));
                    if widgets::secondary_button(ui, &theme, "Choose…").clicked() {
                        pick_steam = true;
                    }
                    if draft.steam_path.is_some()
                        && widgets::link_button(ui, &theme, "Reset to auto").clicked()
                    {
                        draft.steam_path = None;
                    }
                });
                hint(ui, &theme, "Auto-detected from Steam's install locations.");
                row_divider(ui, &theme);

                ui.checkbox(&mut draft.plugin_enabled, "Install the Steam store plugin");
            });
        });

    if pick_download {
        let initial = state.draft.as_ref().map(|draft| draft.download_dir.clone());
        state.download_picker = Some(crate::dialogs::pick_folder(initial));
    }
    if pick_steam {
        let initial = state
            .draft
            .as_ref()
            .and_then(|draft| draft.steam_path.clone())
            .or_else(|| app.snapshot.steam_root.clone());
        state.steam_picker = Some(crate::dialogs::pick_folder(initial));
    }

    state.dirty = save_enabled(state.draft.as_ref(), &current);
    let can_save = state.dirty;

    ui.add_space(8.0);
    widgets::divider(ui, &theme);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(can_save, |ui| {
            if !can_save {
                ui.set_opacity(0.5);
            }
            if widgets::primary_button(ui, &theme, "Save").clicked() {
                save = true;
            }
        });
        ui.add_enabled_ui(can_save, |ui| {
            if !can_save {
                ui.set_opacity(0.5);
            }
            if widgets::secondary_button(ui, &theme, "Reset").clicked() {
                reset = true;
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if can_save {
                widgets::badge(ui, &theme, "Unsaved changes", Tone::Warning);
            } else {
                ui.label(
                    RichText::new("No unsaved changes")
                        .size(theme.small_size)
                        .color(theme.faint),
                );
            }
        });
    });

    if reset {
        state.draft = None;
        state.dirty = false;
    }
    if save && can_save {
        if let Some(draft) = state.draft.take() {
            app.send(Command::SaveSettings(Box::new(draft.sanitized())));
        }
        state.dirty = false;
    }
}

/// A section header plus its card.
fn section(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    widgets::section_label(ui, theme, title);
    ui.add_space(6.0);
    widgets::card(ui, theme, add_contents);
    ui.add_space(16.0);
}

/// A fixed-width label that lines form controls up.
fn field_label(ui: &mut egui::Ui, theme: &Theme, text: &str) -> eframe::egui::Response {
    ui.add_sized(
        [150.0, 24.0],
        Label::new(
            RichText::new(text)
                .size(theme.small_size + 0.5)
                .color(theme.muted),
        ),
    )
}

/// Sets an explicit accessible name on a widget.
fn label_widget(ui: &egui::Ui, id: egui::Id, name: &str) {
    ui.ctx()
        .accesskit_node_builder(id, |node| node.set_label(name));
}

/// A muted explanatory line.
fn hint(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .size(theme.small_size)
            .color(theme.faint),
    );
}

/// A hairline with breathing room between rows.
fn row_divider(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(8.0);
    widgets::divider(ui, theme);
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use luatools_core::{BackendHandle, Settings};

    #[test]
    fn save_is_disabled_without_changes() {
        let mut app = AppState::new(BackendHandle::disconnected(), Settings::default());
        let mut state = State::default();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| show(ui, &mut app, &mut state));
        output.textures_delta.clear();

        assert!(
            state.draft.is_some(),
            "the draft is created on first render"
        );
        assert!(!state.dirty);
        assert!(!save_enabled(state.draft.as_ref(), &app.settings));

        if let Some(draft) = state.draft.as_mut() {
            draft.analytics_enabled = !draft.analytics_enabled;
        }
        assert!(save_enabled(state.draft.as_ref(), &app.settings));
    }
}

//! Per-app management: header actions and the depot table.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Label, Layout, RichText, Sense};
use luatools_core::{AppId, ByteFormat, Command, DepotEntry};
use luatools_i18n::Key;

use crate::pages::Page;
use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// How long to wait before asking for details again when they never arrive.
const DETAILS_RETRY: Duration = Duration::from_secs(3);

/// Manage page transient state.
#[derive(Debug, Default)]
pub struct State {
    /// App currently selected.
    pub selected: Option<AppId>,
    /// Show the depot list instead of the overview.
    pub show_depots: bool,
    /// Last details request, used to avoid one command per frame while loading.
    requested: Option<(AppId, Instant)>,
}

/// Renders the management page.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let title = app.t(Key::Manage_Title);
    let loading_label = app.t(Key::Manage_LoadingDepotInfo);
    let launch_label = app.t(Key::Manage_Action_LaunchOptions);

    widgets::page_header(
        ui,
        &theme,
        title,
        Some("Depots, install folders, and launch settings for one app."),
    );
    ui.add_space(16.0);

    let Some(app_id) = state.selected else {
        widgets::empty_state(
            ui,
            &theme,
            "No app selected",
            "Pick an app on the Builds page to inspect its depots.",
        );
        ui.vertical_centered(|ui| {
            if widgets::secondary_button(ui, &theme, "Open Builds").clicked() {
                app.page = Page::Builds;
            }
        });
        return;
    };

    let Some(details) = app.details(app_id) else {
        let waiting = state
            .requested
            .is_some_and(|(id, at)| id == app_id && at.elapsed() < DETAILS_RETRY);
        if !waiting {
            app.ensure_details(app_id);
            state.requested = Some((app_id, Instant::now()));
        }
        widgets::card(ui, &theme, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.spinner();
                ui.add_space(8.0);
                ui.label(
                    RichText::new(loading_label)
                        .size(theme.small_size + 0.5)
                        .color(theme.muted),
                );
                ui.add_space(24.0);
            });
        });
        // Keep the spinner moving and retry shortly if the backend never answers.
        ui.ctx().request_repaint_after(Duration::from_millis(150));
        return;
    };
    state.requested = None;

    let app_id_text = app_id.to_string();
    let app_id_label = app.tf(Key::Common_AppId, &[&app_id_text]);
    let installed = details.app.installed.is_installed();
    let installed_size = details.app.installed.size_on_disk();
    let install_path: Option<PathBuf> = details.app.installed.path().cloned();

    let mut open_folder = false;
    let mut reveal = false;
    let mut launch = false;
    let mut launch_options = false;
    let mut install = false;
    let mut store = false;

    egui::ScrollArea::vertical()
        .id_salt("manage-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::card(ui, &theme, |ui| {
                ui.horizontal(|ui| {
                    let meta_width = 150.0;
                    let title_width = (ui.available_width() - meta_width).max(200.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(title_width, 50.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.add(
                                Label::new(
                                    RichText::new(&details.app.name)
                                        .size(theme.title_size - 4.0)
                                        .color(theme.text)
                                        .strong(),
                                )
                                .truncate(),
                            );
                            ui.label(
                                RichText::new(&app_id_label)
                                    .size(theme.small_size + 0.5)
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
                        if installed && installed_size > 0 {
                            ui.label(
                                RichText::new(ByteFormat::new(installed_size).to_string())
                                    .size(theme.small_size + 0.5)
                                    .color(theme.muted),
                            );
                        }
                    });
                });

                ui.add_space(12.0);
                widgets::divider(ui, &theme);
                ui.add_space(12.0);

                if let Some(path) = &install_path {
                    widgets::field_row(ui, &theme, "Install folder", &path.display().to_string());
                } else {
                    widgets::field_row(ui, &theme, "Install folder", "Not installed");
                }
                if let Some(install_dir) = &details.install_dir {
                    widgets::field_row(ui, &theme, "Install dir", install_dir);
                }
                widgets::field_row(ui, &theme, "Depots", &details.depots.len().to_string());
                if details.app.installed.is_installed() && installed_size > 0 {
                    widgets::field_row(
                        ui,
                        &theme,
                        "Size on disk",
                        &ByteFormat::new(installed_size).to_string(),
                    );
                }

                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if widgets::primary_button(ui, &theme, launch_label).clicked() {
                        launch_options = true;
                    }
                    if install_path.is_some() {
                        if widgets::secondary_button(ui, &theme, "Open folder").clicked() {
                            open_folder = true;
                        }
                        if widgets::secondary_button(ui, &theme, "Reveal").clicked() {
                            reveal = true;
                        }
                    }
                    if widgets::secondary_button(ui, &theme, "Launch").clicked() {
                        launch = true;
                    }
                    if widgets::secondary_button(ui, &theme, "Install in Steam").clicked() {
                        install = true;
                    }
                    if widgets::link_button(ui, &theme, "Store page").clicked() {
                        store = true;
                    }
                });
            });

            ui.add_space(16.0);
            let depot_count = details.depots.len();
            let total_size = details.total_depot_size();
            widgets::card(ui, &theme, |ui| {
                ui.horizontal(|ui| {
                    widgets::section_label(ui, &theme, "DEPOTS");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{depot_count} depots · {}",
                                ByteFormat::new(total_size)
                            ))
                            .size(theme.small_size + 0.5)
                            .color(theme.muted),
                        );
                    });
                });
                ui.add_space(10.0);

                if details.depots.is_empty() {
                    widgets::empty_state(
                        ui,
                        &theme,
                        "No depot metadata",
                        "This app has no readable depot list.",
                    );
                } else {
                    depot_table(ui, &theme, &details.depots);
                }
            });
        });

    if launch_options {
        app.open_launch_options(app_id);
    }
    if open_folder && let Some(path) = install_path.as_ref() {
        app.send(Command::OpenPath(path.clone()));
    }
    if reveal && let Some(path) = install_path.as_ref() {
        app.send(Command::RevealPath(path.clone()));
    }
    if launch {
        app.send(Command::LaunchApp(app_id));
    }
    if install {
        app.send(Command::InstallApp(app_id));
    }
    if store {
        app.send(Command::OpenStorePage(app_id));
    }
    ui.add_space(8.0);
}

/// Header plus one row per depot, with aligned fixed-width columns.
fn depot_table(ui: &mut egui::Ui, theme: &Theme, depots: &[DepotEntry]) {
    let gap = ui.spacing().item_spacing.x;
    let status_width = 76.0;
    let id_width = 78.0;
    let size_width = 88.0;
    let manifest_width = 178.0;
    let name_width =
        (ui.available_width() - status_width - id_width - size_width - manifest_width - gap * 4.0)
            .max(140.0);

    let heading = |ui: &mut egui::Ui, width: f32, text: &str| {
        table_cell(
            ui,
            width,
            RichText::new(text)
                .size(theme.small_size)
                .color(theme.muted)
                .strong(),
        );
    };
    ui.horizontal(|ui| {
        heading(ui, name_width, "DEPOT");
        heading(ui, id_width, "ID");
        heading(ui, size_width, "SIZE");
        heading(ui, manifest_width, "MANIFEST");
        heading(ui, status_width, "ENABLED");
    });
    ui.add_space(8.0);
    widgets::divider(ui, theme);
    ui.add_space(6.0);

    for (index, depot) in depots.iter().enumerate() {
        if index > 0 {
            widgets::divider(ui, theme);
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            table_cell(
                ui,
                name_width,
                RichText::new(&depot.name)
                    .size(theme.small_size + 0.5)
                    .color(theme.text)
                    .strong(),
            );
            table_cell(
                ui,
                id_width,
                RichText::new(depot.id.to_string())
                    .size(theme.small_size + 0.5)
                    .color(theme.muted),
            );
            let size = depot.size.map_or_else(
                || "—".to_owned(),
                |bytes| ByteFormat::new(bytes).to_string(),
            );
            table_cell(
                ui,
                size_width,
                RichText::new(size)
                    .size(theme.small_size + 0.5)
                    .color(theme.text),
            );
            let manifest = depot
                .manifest_id
                .map_or_else(|| "—".to_owned(), |id| id.to_string());
            table_cell(
                ui,
                manifest_width,
                RichText::new(manifest)
                    .size(theme.small_size)
                    .color(theme.muted)
                    .monospace(),
            );
            enabled_cell(ui, theme, depot.enabled, status_width);
        });
        ui.add_space(6.0);
    }
}

/// A truncated cell with a stable width.
fn table_cell(ui: &mut egui::Ui, width: f32, text: RichText) {
    ui.add_sized([width, 18.0], Label::new(text).truncate());
}

/// A coloured dot and On/Off label.
fn enabled_cell(ui: &mut egui::Ui, theme: &Theme, enabled: bool, width: f32) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 18.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            let colour = if enabled { theme.success } else { theme.faint };
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.5, colour);
            ui.label(
                RichText::new(if enabled { "On" } else { "Off" })
                    .size(theme.small_size)
                    .color(colour),
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use luatools_core::{
        AppDetails, AppEntry, BackendHandle, DepotEntry, DepotId, InstalledState, ManifestId,
        Settings,
    };

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
    fn renders_selected_app_with_depots() {
        let mut app = state();
        let mut entry = AppEntry::new(AppId::new(730), "Counter-Strike 2");
        entry.installed = InstalledState::Installed {
            path: PathBuf::from("/steam/steamapps/common/Counter-Strike 2"),
            size_on_disk: 30_000_000_000,
            build_id: Some("17800000".to_owned()),
        };
        app.snapshot.apps.push(entry.clone());

        let mut details = AppDetails::new(entry);
        details.install_dir = Some("Counter-Strike 2".to_owned());
        details.depots = vec![
            DepotEntry {
                id: DepotId::new(731),
                name: "Counter-Strike 2 Content".to_owned(),
                size: Some(28_500_000_000),
                manifest_id: Some(ManifestId::new(7_649_880_572_861_247_491)),
                optional: false,
                enabled: true,
            },
            DepotEntry {
                id: DepotId::new(732),
                name: "Counter-Strike 2 Linux".to_owned(),
                size: None,
                manifest_id: None,
                optional: true,
                enabled: false,
            },
        ];
        app.details.insert(AppId::new(730), details);

        let mut page_state = State {
            selected: Some(AppId::new(730)),
            ..State::default()
        };
        render(&mut app, &mut page_state);
    }

    #[test]
    fn renders_nothing_selected_state() {
        let mut app = state();
        let mut page_state = State::default();
        render(&mut app, &mut page_state);
    }
}

//! Per-app management: metadata, launch entries, and the depot table.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Label, Layout, RichText, Sense};
use luatools_core::{AppDetails, AppId, ByteFormat, Command, DepotEntry, DepotId};
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
        Some("Depots, install folders, metadata, and launch settings for one app."),
    );
    ui.add_space(16.0);

    if !app.drifted_launches.is_empty() {
        drift_card(ui, app, &theme);
        ui.add_space(16.0);
    }

    let Some(app_id) = state.selected else {
        widgets::empty_state(
            ui,
            &theme,
            "No app selected",
            "Pick an app on the Builds page to inspect its depots.",
        );
        ui.vertical_centered(|ui| {
            if widgets::secondary_button(ui, &theme, app.t(Key::Nav_Builds)).clicked() {
                app.page = Page::Builds;
            }
        });
        return;
    };

    let Some(details) = app.details(app_id).cloned() else {
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
    let mut copy_id = false;
    let mut refresh = false;
    let mut depots = details.depots.clone();

    egui::ScrollArea::vertical()
        .id_salt("manage-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            header_card(
                ui,
                app,
                &theme,
                app_id,
                &details,
                &app_id_label,
                installed,
                installed_size,
                &install_path,
                &mut open_folder,
                &mut reveal,
                &mut launch,
                &mut launch_options,
                &mut install,
                &mut store,
                &mut copy_id,
                &mut refresh,
            );

            if details.metadata.is_some() || details.app.metadata.is_some() {
                ui.add_space(16.0);
                metadata_card(ui, app, &theme, &details);
            }

            ui.add_space(16.0);
            launch_card(ui, app, &theme, &details, launch_label, &mut launch_options);

            ui.add_space(16.0);
            depot_card(ui, app, &theme, &details, &mut depots);
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
    if copy_id {
        ui.ctx().copy_text(app_id_text.clone());
    }
    if refresh {
        app.send(Command::RefreshAppMetadata(app_id));
        app.send(Command::LoadDepotInfo(app_id));
    }

    if depots
        .iter()
        .zip(details.depots.iter())
        .any(|(new, old)| new.enabled != old.enabled)
    {
        let enabled: Vec<DepotId> = depots
            .iter()
            .filter(|depot| depot.enabled)
            .map(|depot| depot.id)
            .collect();
        if let Some(cached) = app.details.get_mut(&app_id) {
            for depot in &mut cached.depots {
                if let Some(updated) = depots.iter().find(|candidate| candidate.id == depot.id) {
                    depot.enabled = updated.enabled;
                }
            }
        }
        app.send(Command::SaveDepotSelection { app_id, enabled });
    }
    ui.add_space(8.0);
}

#[allow(clippy::too_many_arguments)]
fn header_card(
    ui: &mut egui::Ui,
    app: &mut AppState,
    theme: &Theme,
    app_id: AppId,
    details: &AppDetails,
    app_id_label: &str,
    installed: bool,
    installed_size: u64,
    install_path: &Option<PathBuf>,
    open_folder: &mut bool,
    reveal: &mut bool,
    launch: &mut bool,
    launch_options: &mut bool,
    install: &mut bool,
    store: &mut bool,
    copy_id: &mut bool,
    refresh: &mut bool,
) {
    widgets::card(ui, theme, |ui| {
        ui.horizontal(|ui| {
            cover(ui, app, app_id, &details.app.name);
            ui.add_space(10.0);
            let meta_width = 190.0;
            let title_width = (ui.available_width() - meta_width).max(200.0);
            ui.allocate_ui_with_layout(
                egui::vec2(title_width, 52.0),
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
                        RichText::new(app_id_label)
                            .size(theme.small_size + 0.5)
                            .color(theme.muted),
                    );
                    if let Some(metadata) = details.metadata.as_ref() {
                        let mut line = Vec::new();
                        if let Some(date) = metadata.release_date.as_deref() {
                            line.push(date.to_owned());
                        }
                        if !metadata.genres.is_empty() {
                            line.push(metadata.genres.join(", "));
                        }
                        if !line.is_empty() {
                            ui.label(
                                RichText::new(line.join("  ·  "))
                                    .size(theme.small_size)
                                    .color(theme.faint),
                            );
                        }
                    }
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if installed {
                    widgets::badge(
                        ui,
                        theme,
                        app.t(Key::Plugin_Status_Installed),
                        Tone::Success,
                    );
                } else {
                    widgets::badge(
                        ui,
                        theme,
                        app.t(Key::Plugin_Status_NotInstalled),
                        Tone::Muted,
                    );
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
        widgets::divider(ui, theme);
        ui.add_space(12.0);

        if let Some(path) = install_path {
            widgets::field_row(ui, theme, "Install folder", &path.display().to_string());
        } else {
            widgets::field_row(
                ui,
                theme,
                "Install folder",
                app.t(Key::Plugin_Status_NotInstalled),
            );
        }
        if let Some(install_dir) = &details.install_dir {
            widgets::field_row(ui, theme, "Install dir", install_dir);
        }
        widgets::field_row(
            ui,
            theme,
            app.t(Key::Nav_Builds),
            &details.depots.len().to_string(),
        );
        if installed && installed_size > 0 {
            widgets::field_row(
                ui,
                theme,
                "Size on disk",
                &ByteFormat::new(installed_size).to_string(),
            );
        }
        if let Some(info) = details.depot_info.as_ref() {
            if let Some(build) = info.public_build_id.as_deref() {
                widgets::field_row(ui, theme, "Latest build", build);
            }
            widgets::field_row(ui, theme, "DLC", &info.dlc_ids.len().to_string());
        }

        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            if widgets::primary_button(ui, theme, app.t(Key::Manage_Action_LaunchOptions)).clicked()
            {
                *launch_options = true;
            }
            if install_path.is_some() {
                if widgets::secondary_button(ui, theme, app.t(Key::Settings_Open)).clicked() {
                    *open_folder = true;
                }
                if widgets::secondary_button(ui, theme, app.t(Key::Add_Reveal)).clicked() {
                    *reveal = true;
                }
            }
            if widgets::secondary_button(ui, theme, app.t(Key::Manage_Action_OpenInSteam)).clicked()
            {
                *launch = true;
            }
            if installed {
                if widgets::secondary_button(ui, theme, app.t(Key::Nav_RestartSteam)).clicked() {
                    app.confirm = Some(crate::app::confirm(
                        app.t(Key::Manage_RestartSteam_Title),
                        app.t(Key::Main_RestartSteam_Ask),
                        app.t(Key::Nav_RestartSteam),
                        Command::RestartSteam,
                    ));
                }
            } else if widgets::secondary_button(ui, theme, app.t(Key::Plugin_Btn_Install)).clicked()
            {
                *install = true;
            }
            if widgets::secondary_button(ui, theme, app.t(Key::Manage_Refresh)).clicked() {
                *refresh = true;
            }
            if widgets::link_button(ui, theme, app.t(Key::Common_CopyAppId)).clicked() {
                *copy_id = true;
            }
            if widgets::link_button(ui, theme, app.t(Key::Manage_Action_OpenStore)).clicked() {
                *store = true;
            }
        });
    });
}

fn drift_card(ui: &mut egui::Ui, app: &mut AppState, theme: &Theme) {
    let count = app.drifted_launches.len();
    widgets::card(ui, theme, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "Steam overwrote staged launch edits for {count} app(s)."
                ))
                .size(theme.small_size + 0.5)
                .color(theme.warning),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::primary_button(ui, theme, "Re-apply launch edits").clicked() {
                    app.confirm = Some(crate::app::confirm(
                        "Re-apply launch edits?",
                        "Steam will be closed and relaunched so appinfo.vdf can be rewritten.",
                        "Re-apply",
                        Command::ReapplyLaunchOptions,
                    ));
                }
            });
        });
    });
}

fn metadata_card(ui: &mut egui::Ui, app: &AppState, theme: &Theme, details: &AppDetails) {
    let metadata = details.metadata.as_ref().or(details.app.metadata.as_ref());
    let Some(metadata) = metadata else {
        return;
    };
    widgets::card(ui, theme, |ui| {
        widgets::section_label(ui, theme, &app.t(Key::Manage_AboutHeader).to_uppercase());
        ui.add_space(8.0);
        if let Some(description) = metadata.short_description.as_deref()
            && !description.trim().is_empty()
        {
            ui.label(
                RichText::new(description.trim())
                    .size(theme.small_size + 0.5)
                    .color(theme.text),
            );
            ui.add_space(8.0);
        }
        if !metadata.developers.is_empty() {
            widgets::field_row(
                ui,
                theme,
                app.t(Key::Manage_Info_Developer),
                &metadata.developer_label(),
            );
        }
        if metadata.has_distinct_publisher() {
            widgets::field_row(
                ui,
                theme,
                app.t(Key::Manage_Info_Publisher),
                &metadata.publisher_label(),
            );
        }
        if !metadata.genres.is_empty() {
            widgets::field_row(
                ui,
                theme,
                app.t(Key::Manage_Filter_Genre),
                &metadata.genres.join(", "),
            );
        }
        if let Some(year) = metadata.release_year {
            widgets::field_row(ui, theme, "Released", &year.to_string());
        }
        if let Some(score) = metadata.metacritic {
            widgets::field_row(
                ui,
                theme,
                app.t(Key::Manage_Sort_Metacritic),
                &score.to_string(),
            );
        }
        if let Some(reviews) = metadata.reviews {
            widgets::field_row(ui, theme, "Reviews", &reviews.to_string());
        }
    });
}

fn launch_card(
    ui: &mut egui::Ui,
    app: &AppState,
    theme: &Theme,
    details: &AppDetails,
    launch_label: &str,
    launch_options: &mut bool,
) {
    widgets::card(ui, theme, |ui| {
        ui.horizontal(|ui| {
            widgets::section_label(ui, theme, &app.t(Key::Launch_Title).to_uppercase());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::secondary_button(ui, theme, launch_label).clicked() {
                    *launch_options = true;
                }
            });
        });
        ui.add_space(8.0);
        if details.launch_entries.is_empty() {
            ui.label(
                RichText::new("No Play-button entries are declared by this app.")
                    .size(theme.small_size + 0.5)
                    .color(theme.muted),
            );
        } else {
            for entry in &details.launch_entries {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(entry.summary())
                            .size(theme.small_size + 0.5)
                            .color(theme.text)
                            .monospace(),
                    );
                    if !entry.description.trim().is_empty() {
                        ui.label(
                            RichText::new(&entry.description)
                                .size(theme.small_size)
                                .color(theme.muted),
                        );
                    }
                });
            }
        }
        if !details.launch_options.trim().is_empty() {
            ui.add_space(8.0);
            widgets::field_row(ui, theme, "User options", details.launch_options.trim());
        }
    });
}

fn depot_card(
    ui: &mut egui::Ui,
    app: &AppState,
    theme: &Theme,
    details: &AppDetails,
    depots: &mut [DepotEntry],
) {
    let depot_count = depots.len();
    let total_size = details.total_depot_size();
    widgets::card(ui, theme, |ui| {
        ui.horizontal(|ui| {
            widgets::section_label(ui, theme, app.t(Key::Add_Depots));
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

        if depots.is_empty() {
            widgets::empty_state(
                ui,
                theme,
                "No depot metadata",
                "This app has no readable depot list.",
            );
            return;
        }
        depot_table(ui, app, theme, depots);
    });
}

/// Header plus one row per depot, with aligned fixed-width columns.
fn depot_table(ui: &mut egui::Ui, app: &AppState, theme: &Theme, depots: &mut [DepotEntry]) {
    let gap = ui.spacing().item_spacing.x;
    let enabled_width = 58.0;
    let id_width = 72.0;
    let size_width = 82.0;
    let meta_width = 150.0;
    let manifest_width = 160.0;
    let name_width = (ui.available_width()
        - enabled_width
        - id_width
        - size_width
        - meta_width
        - manifest_width
        - gap * 5.0)
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
        heading(ui, name_width, &app.t(Key::Manage_Depot).to_uppercase());
        heading(ui, id_width, "ID");
        heading(ui, size_width, "SIZE");
        heading(ui, meta_width, "OS / LANGUAGE");
        heading(ui, manifest_width, "PUBLIC MANIFEST");
        heading(ui, enabled_width, "ON");
    });
    ui.add_space(6.0);
    widgets::divider(ui, theme);
    ui.add_space(6.0);

    for (index, depot) in depots.iter_mut().enumerate() {
        if index > 0 {
            widgets::divider(ui, theme);
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            let mut label = depot.name.clone();
            if depot.is_shared {
                label.push_str(&format!("  ({})", app.t(Key::Manage_SharedDepot)));
            }
            if let Some(dlc) = depot.dlc_app_id {
                label.push_str(&format!(
                    "  ({})",
                    app.tf(Key::Manage_DlcName, &[&dlc.to_string()])
                ));
            }
            table_cell(
                ui,
                name_width,
                RichText::new(label)
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
            let mut meta = Vec::new();
            if let Some(os) = &depot.os {
                meta.push(os.as_str());
            }
            if let Some(language) = &depot.language {
                meta.push(language.as_str());
            }
            table_cell(
                ui,
                meta_width,
                RichText::new(if meta.is_empty() {
                    "—".to_owned()
                } else {
                    meta.join(" · ")
                })
                .size(theme.small_size)
                .color(theme.muted),
            );
            let manifest = depot
                .public_manifest_id
                .or(depot.manifest_id)
                .map_or_else(|| "—".to_owned(), |id| id.to_string());
            table_cell(
                ui,
                manifest_width,
                RichText::new(manifest)
                    .size(theme.small_size)
                    .color(theme.muted)
                    .monospace(),
            );
            ui.allocate_ui_with_layout(
                egui::vec2(enabled_width, 18.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.checkbox(&mut depot.enabled, "");
                },
            );
        });
        ui.add_space(4.0);
    }
}

/// A truncated cell with a stable width.
fn table_cell(ui: &mut egui::Ui, width: f32, text: RichText) {
    ui.add_sized([width, 18.0], Label::new(text).truncate());
}

fn cover(ui: &mut egui::Ui, app: &mut AppState, app_id: AppId, name: &str) {
    let theme = app.theme;
    let path = app
        .details(app_id)
        .and_then(|details| details.app.cover_path.clone());
    if let Some(texture) = app.cover_texture(ui.ctx(), app_id, path.as_deref()) {
        ui.add(
            egui::Image::new((texture.id(), egui::vec2(92.0, 43.0)))
                .corner_radius(theme.radius_control),
        );
        return;
    }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(92.0, 43.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, theme.radius_control, theme.bg);
    let initial = name
        .chars()
        .find(|c| !c.is_whitespace())
        .map_or('?', |c| c.to_ascii_uppercase());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(18.0),
        theme.accent,
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
            {
                let mut depot = DepotEntry::new(DepotId::new(731), "Counter-Strike 2 Content");
                depot.size = Some(28_500_000_000);
                depot.public_manifest_id = Some(ManifestId::new(7_649_880_572_861_247_491));
                depot
            },
            {
                let mut depot = DepotEntry::new(DepotId::new(732), "Counter-Strike 2 Linux");
                depot.optional = true;
                depot.enabled = false;
                depot.os = Some("linux".to_owned());
                depot
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

//! Home dashboard: library and transfer summaries at a glance.

use std::path::Path;

use eframe::egui::{self, Align, Layout, RichText};
use luatools_core::{AppEntry, ByteFormat, Command};
use luatools_i18n::Key;

use crate::pages::Page;
use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// How many library apps the dashboard lists under "recently added".
const RECENT_LIMIT: usize = 6;

/// Home page transient state.
#[derive(Debug, Default)]
pub struct State {}

/// Renders the dashboard.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, _state: &mut State) {
    let theme = app.theme;
    let welcome = app.t(Key::Home_Welcome);
    let tagline = "Your Steam library at a glance.";

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            widgets::page_header(ui, &theme, welcome, Some(tagline));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::secondary_button(ui, &theme, app.t(Key::Nav_Add)).clicked() {
                app.page = Page::Add;
            }
            if app.is_busy("library") {
                ui.spinner();
                ui.label(
                    RichText::new(app.t(Key::Home_CheckingSteam))
                        .size(theme.small_size + 0.5)
                        .color(theme.muted),
                );
            } else if widgets::secondary_button(ui, &theme, app.t(Key::Manage_Refresh)).clicked() {
                app.send(Command::RefreshLibrary);
            }
        });
    });
    ui.add_space(16.0);

    egui::ScrollArea::vertical()
        .id_salt("home-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            stats_card(ui, app);
            ui.add_space(16.0);
            status_card(ui, app);
            ui.add_space(16.0);
            downloads_card(ui, app);
            ui.add_space(16.0);
            drop_hint(ui, app);
            ui.add_space(16.0);
            recent_card(ui, app);
            ui.add_space(8.0);
        });
}

/// The four summary tiles, inside one card.
fn stats_card(ui: &mut egui::Ui, app: &AppState) {
    let theme = app.theme;
    let library = app.snapshot.apps.len().to_string();
    let installed = app.snapshot.installed_count().to_string();
    let active = app.active_downloads().count().to_string();
    let steam_root = app.snapshot.steam_root.as_deref().map_or_else(
        || app.t(Key::Settings_SteamSource_NotFound).to_owned(),
        compact_path,
    );
    let steam_found = app.snapshot.steam_root.is_some();

    widgets::card(ui, &theme, |ui| {
        ui.columns(4, |columns| {
            stat_tile(&mut columns[0], &theme, "Library apps", &library);
            stat_tile(
                &mut columns[1],
                &theme,
                app.t(Key::Plugin_Status_Installed),
                &installed,
            );
            stat_tile(&mut columns[2], &theme, "Active downloads", &active);
            stat_tile(
                &mut columns[3],
                &theme,
                app.t(Key::Settings_SteamLocation),
                &steam_root,
            );
        });
        if !steam_found {
            ui.add_space(10.0);
            ui.label(
                RichText::new(app.t(Key::Home_SteamNotFound))
                    .size(theme.small_size)
                    .color(theme.warning),
            );
        }
    });
}

/// One inset summary tile.
fn stat_tile(ui: &mut egui::Ui, theme: &Theme, label: &str, value: &str) {
    egui::Frame::new()
        .fill(theme.bg)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(theme.radius_control)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(
                    RichText::new(value)
                        .size(theme.title_size - 4.0)
                        .color(theme.text)
                        .strong(),
                )
                .truncate(),
            );
            ui.label(
                RichText::new(label)
                    .size(theme.small_size)
                    .color(theme.muted),
            );
        });
}

/// Steam, account, and plugin status rows. Each row opens the page where it can be changed.
fn status_card(ui: &mut egui::Ui, app: &mut AppState) {
    let theme = app.theme;
    let steam = app.snapshot.steam_root.as_ref().map_or_else(
        || app.t(Key::Home_SteamNotFound).to_owned(),
        |root| app.tf(Key::Home_SteamDetected, &[&root.display().to_string()]),
    );
    let account = match &app.auth {
        luatools_core::AuthState::SignedIn { profile } => {
            let name = profile
                .display_name
                .as_deref()
                .or(profile.email.as_deref())
                .unwrap_or("Account");
            app.tf(Key::Home_SignedInAs, &[name])
        }
        luatools_core::AuthState::WaitingForBrowser { .. } => {
            app.t(Key::Nav_WaitingForBrowser).to_owned()
        }
        luatools_core::AuthState::Failed { .. } | luatools_core::AuthState::Guest => {
            app.t(Key::Home_BrowsingAsGuest).to_owned()
        }
    };
    let plugin = match &app.plugin {
        luatools_core::PluginStatus::Installed { version } => {
            format!("{} · {version}", app.t(Key::Plugin_Status_Installed))
        }
        luatools_core::PluginStatus::UpdateAvailable { .. } => {
            app.t(Key::Plugin_Badge_UpdateAvailable).to_owned()
        }
        luatools_core::PluginStatus::Working { step } => step.clone(),
        luatools_core::PluginStatus::Failed { reason } => reason.clone(),
        luatools_core::PluginStatus::NotInstalled => {
            app.t(Key::Plugin_Status_NotInstalled).to_owned()
        }
    };
    let mut settings = false;
    let mut plugin_page = false;
    let mut install = false;
    widgets::card(ui, &theme, |ui| {
        status_row(ui, &theme, "Steam", &steam, true, &mut settings);
        widgets::divider(ui, &theme);
        status_row(ui, &theme, "Account", &account, true, &mut settings);
        widgets::divider(ui, &theme);
        status_row(
            ui,
            &theme,
            app.t(Key::Home_PluginStatus),
            &plugin,
            true,
            &mut plugin_page,
        );
        if matches!(app.plugin, luatools_core::PluginStatus::NotInstalled)
            && widgets::link_button(ui, &theme, app.t(Key::Plugin_Btn_Install)).clicked()
        {
            install = true;
        }
    });
    if settings {
        app.page = Page::Settings;
    }
    if plugin_page {
        app.page = Page::Plugin;
    }
    if install {
        app.send(Command::InstallPlugin);
    }
}

fn status_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    value: &str,
    clickable: bool,
    clicked: &mut bool,
) {
    let response = ui.add(
        egui::Button::new(
            RichText::new(format!("{label}  ·  {value}"))
                .size(theme.small_size + 0.5)
                .color(theme.text),
        )
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(ui.available_width(), 28.0)),
    );
    if clickable && response.clicked() {
        *clicked = true;
    }
}

/// The link half of the original drop zone. File installation is out of scope, but dragging a
/// Steam/store/SteamDB link is a supported way to open an app.
fn drop_hint(ui: &mut egui::Ui, app: &mut AppState) {
    let theme = app.theme;
    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(app.t(Key::Drop_LinkHint))
                    .size(theme.small_size + 0.5)
                    .color(theme.muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::link_button(ui, &theme, app.t(Key::Nav_Add)).clicked() {
                    app.page = Page::Add;
                }
            });
        });
    });
}

/// Active transfers with a progress bar per job.
fn downloads_card(ui: &mut egui::Ui, app: &mut AppState) {
    let theme = app.theme;
    let mut open_downloads = false;

    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            widgets::section_label(ui, &theme, "ACTIVE DOWNLOADS");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::link_button(ui, &theme, app.t(Key::Nav_Downloads)).clicked() {
                    open_downloads = true;
                }
                let active = app.active_downloads().count();
                if active > 0 {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("{active} in progress"))
                            .size(theme.small_size)
                            .color(theme.faint),
                    );
                }
            });
        });
        ui.add_space(10.0);

        let jobs: Vec<_> = app.active_downloads().collect();
        if jobs.is_empty() {
            widgets::empty_state(
                ui,
                &theme,
                "Nothing downloading",
                "Queued depot transfers appear here with live progress.",
            );
            return;
        }

        for (index, job) in jobs.iter().enumerate() {
            if index > 0 {
                ui.add_space(10.0);
            }
            ui.horizontal(|ui| {
                let meta_width = 190.0;
                let name_width = (ui.available_width() - meta_width).max(140.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(name_width, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&job.request.app_name)
                                    .size(theme.text_size)
                                    .color(theme.text)
                                    .strong(),
                            )
                            .truncate(),
                        );
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(job.progress_label())
                            .size(theme.small_size + 0.5)
                            .color(theme.muted),
                    );
                });
            });
            ui.add_space(5.0);
            widgets::progress_bar(ui, &theme, job.fraction());
        }
    });

    if open_downloads {
        app.page = Page::Downloads;
    }
}

/// Up to six library apps, installed entries first.
fn recent_card(ui: &mut egui::Ui, app: &mut AppState) {
    let theme = app.theme;
    let heading = app.t(Key::Home_RecentlyAdded).to_uppercase();
    let details_label = app.t(Key::Home_ViewDetails);
    let mut open: Option<luatools_core::AppId> = None;

    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            widgets::section_label(ui, &theme, &heading);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !app.snapshot.apps.is_empty() {
                    let total = app.snapshot.apps.len();
                    ui.label(
                        RichText::new(format!("{total} in library"))
                            .size(theme.small_size)
                            .color(theme.faint),
                    );
                }
            });
        });
        ui.add_space(10.0);

        if app.snapshot.apps.is_empty() {
            widgets::empty_state(
                ui,
                &theme,
                "No library yet",
                "Refresh to scan the Steam libraries on this computer.",
            );
            return;
        }

        let mut recent: Vec<AppEntry> = app.snapshot.apps.to_vec();
        recent.sort_by_key(|entry| !entry.installed.is_installed());
        recent.truncate(RECENT_LIMIT);

        for (index, entry) in recent.iter().enumerate() {
            if index > 0 {
                ui.add_space(6.0);
                widgets::divider(ui, &theme);
                ui.add_space(6.0);
            }
            let mut choice = false;
            ui.horizontal(|ui| {
                recent_cover(ui, app, entry);
                ui.add_space(10.0);
                let meta_width = 280.0;
                let name_width = (ui.available_width() - meta_width).max(160.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(name_width, 40.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&entry.name)
                                    .size(theme.text_size)
                                    .color(theme.text)
                                    .strong(),
                            )
                            .truncate(),
                        );
                        ui.label(
                            RichText::new(
                                app.tf(Key::Common_AppFallback, &[&entry.id.to_string()]),
                            )
                            .size(theme.small_size)
                            .color(theme.muted),
                        );
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::link_button(ui, &theme, details_label).clicked() {
                        choice = true;
                    }
                    if entry.installed.is_installed() {
                        widgets::badge(
                            ui,
                            &theme,
                            app.t(Key::Plugin_Status_Installed),
                            Tone::Success,
                        );
                        let size = entry.installed.size_on_disk();
                        if size > 0 {
                            ui.label(
                                RichText::new(ByteFormat::new(size).to_string())
                                    .size(theme.small_size)
                                    .color(theme.muted),
                            );
                        }
                    } else {
                        widgets::badge(
                            ui,
                            &theme,
                            app.t(Key::Plugin_Status_NotInstalled),
                            Tone::Muted,
                        );
                    }
                });
            });
            if choice {
                open = Some(entry.id);
            }
        }
    });

    if let Some(id) = open {
        app.pages.manage.selected = Some(id);
        app.ensure_details(id);
        app.page = Page::Manage;
    }
}

fn recent_cover(ui: &mut egui::Ui, app: &mut AppState, entry: &AppEntry) {
    let theme = app.theme;
    if let Some(texture) = app.cover_texture(ui.ctx(), entry.id, entry.cover_path.as_deref()) {
        ui.add(
            egui::Image::new((texture.id(), egui::vec2(72.0, 34.0)))
                .corner_radius(theme.radius_control),
        );
        return;
    }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(72.0, 34.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, theme.radius_control, theme.bg);
    let initial = entry
        .name
        .chars()
        .find(|ch| !ch.is_whitespace())
        .map_or('?', |ch| ch.to_ascii_uppercase());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(15.0),
        theme.accent,
    );
}

/// Keeps the tail of a long path so the last folders stay readable in a tile.
fn compact_path(path: &Path) -> String {
    const MAX_CHARS: usize = 30;
    let text = path.display().to_string();
    let count = text.chars().count();
    if count <= MAX_CHARS {
        return text;
    }
    let tail: String = text.chars().skip(count - (MAX_CHARS - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use luatools_core::{
        AppId, BackendHandle, DownloadId, DownloadKind, DownloadRequest, DownloadSnapshot,
        DownloadStatus, InstalledState, Settings,
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

    fn installed(id: u32, name: &str, size: u64) -> AppEntry {
        let mut entry = AppEntry::new(AppId::new(id), name);
        entry.installed = InstalledState::Installed {
            path: PathBuf::from(format!("/steam/steamapps/common/{name}")),
            size_on_disk: size,
            build_id: Some("1234".to_owned()),
        };
        entry
    }

    #[test]
    fn renders_populated_dashboard() {
        let mut app = state();
        app.snapshot.apps = vec![
            installed(730, "Counter-Strike 2", 30_000_000_000),
            installed(570, "Dota 2", 45_000_000_000),
            AppEntry::new(AppId::new(400), "Portal"),
        ];
        app.snapshot.steam_root = Some(PathBuf::from("/home/player/.local/share/Steam"));

        let request = DownloadRequest {
            app_id: AppId::new(730),
            app_name: "Counter-Strike 2".to_owned(),
            kind: DownloadKind::Depot,
            depots: Vec::new(),
            url: None,
            destination: PathBuf::from("/steam/steamapps"),
            verify: false,
        };
        let mut job = DownloadSnapshot::queued(DownloadId::new(1), request, Utc::now());
        job.status = DownloadStatus::Downloading;
        job.received_bytes = 1_400_000_000;
        job.total_bytes = Some(2_000_000_000);
        job.bytes_per_second = 4_200_000;
        app.downloads.push(job);

        let mut page_state = State::default();
        render(&mut app, &mut page_state);
    }

    #[test]
    fn renders_empty_dashboard() {
        let mut app = state();
        let mut page_state = State::default();
        render(&mut app, &mut page_state);
    }

    #[test]
    fn long_steam_paths_keep_their_tail() {
        assert_eq!(compact_path(Path::new("/steam")), "/steam");
        let compact = compact_path(Path::new("/home/player/.local/share/Steam"));
        assert_eq!(compact.chars().count(), 30);
        assert!(compact.starts_with('…'));
        assert!(compact.ends_with("share/Steam"));
    }
}

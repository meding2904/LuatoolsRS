//! Store catalogue: search, featured strips, and app details.

use eframe::egui::{self, Align, Layout, RichText};
use luatools_core::{AppId, AppMetadata, Command, StoreSearchItem};
use luatools_i18n::Key;

use crate::pages::Page;
use crate::state::AppState;
use crate::widgets;

/// Add/store page transient state.
#[derive(Debug, Default)]
pub struct State {
    /// Search text.
    pub query: String,
    /// Last query sent to the backend.
    pub last_query: String,
    /// Search hits.
    pub results: Vec<StoreSearchItem>,
    /// Featured strips.
    pub featured: luatools_core::Featured,
    /// Selected store app.
    pub selected: Option<AppId>,
    /// Whether a search is in flight.
    pub searching: bool,
    /// Whether the featured request has been sent.
    pub featured_requested: bool,
}

/// Renders the store page.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let title = app.t(Key::Add_Title);
    let subtitle = app.t(Key::Add_Subtitle);
    widgets::page_header(ui, &theme, title, Some(subtitle));
    ui.add_space(14.0);

    if !state.featured_requested {
        state.featured_requested = true;
        app.send(Command::LoadFeatured);
    }

    let mut search = false;
    ui.horizontal(|ui| {
        let button_width = 92.0;
        let width = (ui.available_width() - button_width - theme.gap).max(200.0);
        ui.allocate_ui_with_layout(
            egui::vec2(width, 30.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                let response = widgets::search_field(
                    ui,
                    &theme,
                    &mut state.query,
                    app.t(Key::Add_SearchPlaceholder),
                );
                search |=
                    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            },
        );
        if widgets::primary_button(ui, &theme, app.t(Key::Add_Fetch)).clicked() {
            search = true;
        }
    });
    if search {
        let query = state.query.trim().to_owned();
        state.last_query = query.clone();
        state.searching = true;
        app.send(Command::SearchStore(query));
    }

    ui.add_space(12.0);
    egui::ScrollArea::vertical()
        .id_salt("add-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !state.featured.is_empty() {
                featured_strip(
                    ui,
                    app,
                    &state.featured.top_sellers,
                    Key::Add_Featured_TopSellers,
                );
                ui.add_space(12.0);
                featured_strip(
                    ui,
                    app,
                    &state.featured.new_releases,
                    Key::Add_Featured_NewReleases,
                );
                ui.add_space(16.0);
            }

            if !state.results.is_empty() {
                widgets::section_label(ui, &theme, "RESULTS");
                ui.add_space(8.0);
                for item in state.results.clone() {
                    if result_row(ui, app, &item).clicked() {
                        state.selected = Some(item.app_id);
                        app.send(Command::LoadStoreDetails(item.app_id));
                    }
                    ui.add_space(6.0);
                }
                ui.add_space(12.0);
            } else if state.searching {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(app.t(Key::Add_Checking))
                            .size(theme.small_size + 0.5)
                            .color(theme.muted),
                    );
                });
            } else if !state.last_query.is_empty() {
                widgets::empty_state(
                    ui,
                    &theme,
                    app.t(Key::Manage_Empty_NoMatch),
                    app.t(Key::Manage_Empty_NoSteam),
                );
            }

            if let Some(app_id) = state.selected {
                store_details(ui, app, app_id);
            }
        });
}

fn featured_strip(
    ui: &mut egui::Ui,
    app: &mut AppState,
    items: &[luatools_core::FeaturedItem],
    heading: Key,
) {
    let theme = app.theme;
    let title = app.t(heading);
    widgets::card(ui, &theme, |ui| {
        widgets::section_label(ui, &theme, &title.to_uppercase());
        ui.add_space(8.0);
        egui::ScrollArea::horizontal()
            .id_salt(("featured", heading))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for item in items {
                        let response = ui.add(
                            egui::Button::new(
                                RichText::new(&item.name)
                                    .size(theme.small_size + 0.5)
                                    .color(theme.text),
                            )
                            .min_size(egui::vec2(160.0, 44.0))
                            .fill(theme.bg)
                            .stroke(egui::Stroke::new(1.0, theme.border))
                            .corner_radius(theme.radius_control),
                        );
                        if response.clicked() {
                            app.send(Command::LoadStoreDetails(item.app_id));
                        }
                        ui.add_space(6.0);
                    }
                });
            });
    });
}

fn result_row(ui: &mut egui::Ui, app: &AppState, item: &StoreSearchItem) -> egui::Response {
    let theme = app.theme;
    let installed = app
        .snapshot
        .app(item.app_id)
        .is_some_and(|entry| entry.installed.is_installed());
    widgets::clickable_card(ui, &theme, item.app_id, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&item.name)
                        .size(theme.text_size + 0.5)
                        .color(theme.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(app.tf(Key::Common_AppFallback, &[&item.app_id.to_string()]))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if installed {
                    widgets::badge(
                        ui,
                        &theme,
                        app.t(Key::Plugin_Status_Installed),
                        crate::theme::Tone::Success,
                    );
                }
            });
        });
    })
}

fn store_details(ui: &mut egui::Ui, app: &mut AppState, app_id: AppId) {
    let theme = app.theme;
    let Some(details) = app.details(app_id).cloned() else {
        return;
    };
    let metadata = details.metadata.clone().unwrap_or_default();
    let installed = details.app.installed.is_installed();
    let mut install = false;
    let mut manage = false;
    let mut store = false;

    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            cover(ui, app, app_id, &details.app.name);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&details.app.name)
                        .size(theme.title_size - 4.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(app.tf(Key::Common_AppFallback, &[&app_id.to_string()]))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
                metadata_line(ui, &theme, &metadata);
            });
        });
        ui.add_space(10.0);
        widgets::divider(ui, &theme);
        ui.add_space(10.0);

        if let Some(description) = metadata.short_description.as_deref()
            && !description.trim().is_empty()
        {
            ui.label(
                RichText::new(description.trim())
                    .size(theme.small_size + 0.5)
                    .color(theme.text),
            );
            ui.add_space(10.0);
        }

        ui.horizontal_wrapped(|ui| {
            if installed {
                if widgets::primary_button(ui, &theme, app.t(Key::Manage_Action_OpenInSteam))
                    .clicked()
                {
                    install = true;
                }
                if widgets::secondary_button(ui, &theme, app.t(Key::Manage_Title)).clicked() {
                    manage = true;
                }
            } else if widgets::primary_button(ui, &theme, app.t(Key::Add_Download)).clicked() {
                install = true;
            }
            if widgets::secondary_button(ui, &theme, app.t(Key::Manage_Action_OpenStore)).clicked()
            {
                store = true;
            }
            if widgets::link_button(ui, &theme, "SteamDB").clicked() {
                app.send(Command::OpenUrl(format!(
                    "https://steamdb.info/app/{app_id}/"
                )));
            }
        });
    });

    if install {
        if installed {
            app.send(Command::LaunchApp(app_id));
        } else {
            app.send(Command::InstallApp(app_id));
        }
    }
    if manage {
        app.pages.manage.selected = Some(app_id);
        // The store record has no appinfo launch entries or local depot metadata; force a
        // fresh local load so the Manage page is complete.
        app.details.remove(&app_id);
        app.ensure_details(app_id);
        app.page = Page::Manage;
    }
    if store {
        app.send(Command::OpenStorePage(app_id));
    }
}

fn metadata_line(ui: &mut egui::Ui, theme: &crate::theme::Theme, metadata: &AppMetadata) {
    let mut parts = Vec::new();
    if let Some(date) = metadata.release_date.as_deref()
        && !date.trim().is_empty()
    {
        parts.push(date.trim().to_owned());
    }
    if !metadata.developers.is_empty() {
        parts.push(metadata.developer_label());
    }
    if metadata.has_distinct_publisher() {
        parts.push(metadata.publisher_label());
    }
    if !parts.is_empty() {
        ui.label(
            RichText::new(parts.join("  ·  "))
                .size(theme.small_size)
                .color(theme.muted),
        );
    }
    if !metadata.genres.is_empty() {
        ui.label(
            RichText::new(metadata.genres.join("  ·  "))
                .size(theme.small_size)
                .color(theme.faint),
        );
    }
}

fn cover(ui: &mut egui::Ui, app: &mut AppState, app_id: AppId, name: &str) {
    let theme = app.theme;
    let path = app
        .details(app_id)
        .and_then(|details| details.app.cover_path.clone());
    if let Some(texture) = app.cover_texture(ui.ctx(), app_id, path.as_deref()) {
        let size = egui::vec2(92.0, 43.0);
        ui.add(egui::Image::new((texture.id(), size)).corner_radius(theme.radius_control));
    } else {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(92.0, 43.0), egui::Sense::hover());
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
}

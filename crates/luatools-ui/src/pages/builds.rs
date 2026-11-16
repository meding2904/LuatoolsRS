//! Library browser: search, filters, sorting, paging, and app selection.

use eframe::egui::{self, Align, Layout, RichText};
use luatools_core::{AppEntry, AppMetadata, ByteFormat};
use luatools_i18n::Key;

use crate::pages::Page;
use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// Operating-system filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OsFilter {
    /// Any platform.
    #[default]
    Any,
    /// Windows only.
    Windows,
    /// macOS only.
    Mac,
    /// Linux only.
    Linux,
}

impl OsFilter {
    const ALL: [Self; 4] = [Self::Any, Self::Windows, Self::Mac, Self::Linux];

    const fn label(self) -> &'static str {
        match self {
            Self::Any => "Any OS",
            Self::Windows => "Windows",
            Self::Mac => "macOS",
            Self::Linux => "Linux",
        }
    }

    fn matches(self, metadata: Option<&AppMetadata>) -> bool {
        let Some(metadata) = metadata else {
            return self == Self::Any;
        };
        match self {
            Self::Any => true,
            Self::Windows => metadata.windows,
            Self::Mac => metadata.mac,
            Self::Linux => metadata.linux,
        }
    }
}

/// App-type filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindFilter {
    /// Any product type.
    #[default]
    Any,
    /// Games only.
    Game,
    /// Downloadable content only.
    Dlc,
    /// Applications and tools.
    App,
}

impl KindFilter {
    const ALL: [Self; 4] = [Self::Any, Self::Game, Self::Dlc, Self::App];

    const fn label(self) -> &'static str {
        match self {
            Self::Any => "Any type",
            Self::Game => "Game",
            Self::Dlc => "DLC",
            Self::App => "Application",
        }
    }

    fn matches(self, metadata: Option<&AppMetadata>) -> bool {
        let Some(metadata) = metadata else {
            return self == Self::Any;
        };
        let kind = metadata.app_type.as_deref().unwrap_or_default();
        match self {
            Self::Any => true,
            Self::Game => kind.eq_ignore_ascii_case("game"),
            Self::Dlc => kind.eq_ignore_ascii_case("dlc"),
            Self::App => {
                kind.eq_ignore_ascii_case("application")
                    || kind.eq_ignore_ascii_case("tool")
                    || kind.eq_ignore_ascii_case("demo")
            }
        }
    }
}

/// Price filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriceFilter {
    /// Any price.
    #[default]
    Any,
    /// Free-to-play only.
    Free,
    /// Paid only.
    Paid,
}

impl PriceFilter {
    fn matches(self, metadata: Option<&AppMetadata>) -> bool {
        let Some(metadata) = metadata else {
            return self == Self::Any;
        };
        match self {
            Self::Any => true,
            Self::Free => metadata.is_free,
            Self::Paid => !metadata.is_free,
        }
    }
}

/// Content filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentFilter {
    /// Show everything.
    #[default]
    Any,
    /// Hide adult-only apps.
    HideAdult,
    /// Show only adult-only apps.
    AdultOnly,
}

impl ContentFilter {
    fn matches(self, metadata: Option<&AppMetadata>) -> bool {
        let adult = metadata.is_some_and(|metadata| metadata.is_adult);
        match self {
            Self::Any => true,
            Self::HideAdult => !adult,
            Self::AdultOnly => adult,
        }
    }
}

/// Sort order for the library list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Alphabetical by name.
    #[default]
    Name,
    /// Newest release first.
    Release,
    /// Highest Metacritic score first.
    Metacritic,
    /// Most recommendations first.
    Reviews,
}

impl Sort {
    const ALL: [Self; 4] = [Self::Name, Self::Release, Self::Metacritic, Self::Reviews];

    const fn label(self) -> Key {
        match self {
            Self::Name => Key::Manage_Sort_NameAZ,
            Self::Release => Key::Manage_Sort_ReleaseNewest,
            Self::Metacritic => Key::Manage_Sort_Metacritic,
            Self::Reviews => Key::Manage_Sort_MostReviewed,
        }
    }
}

/// Builds page transient state.
#[derive(Debug)]
pub struct State {
    /// Show only installed apps.
    pub installed_only: bool,
    /// Operating-system filter.
    pub os: OsFilter,
    /// Genre filter; empty means any.
    pub genre: String,
    /// Product-type filter.
    pub kind: KindFilter,
    /// Release-year filter.
    pub year: Option<i32>,
    /// Price filter.
    pub price: PriceFilter,
    /// Adult-content filter.
    pub content: ContentFilter,
    /// Sort order.
    pub sort: Sort,
    /// Zero-based page.
    pub page: usize,
    /// Rows per page; zero means all.
    pub page_size: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            installed_only: false,
            os: OsFilter::Any,
            genre: String::new(),
            kind: KindFilter::Any,
            year: None,
            price: PriceFilter::Any,
            content: ContentFilter::Any,
            sort: Sort::Name,
            page: 0,
            page_size: 24,
        }
    }
}

impl State {
    /// Clears every filter.
    pub fn clear(&mut self) {
        self.installed_only = false;
        self.os = OsFilter::Any;
        self.genre.clear();
        self.kind = KindFilter::Any;
        self.year = None;
        self.price = PriceFilter::Any;
        self.content = ContentFilter::Any;
        self.sort = Sort::Name;
        self.page = 0;
        self.page_size = 24;
    }
}

/// Renders the library browser.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let search_hint = app.t(Key::Common_SearchPlaceholder);
    let clear_label = app.t(Key::Manage_ClearFilters);
    let no_match = app.t(Key::Manage_Empty_NoMatch);
    let filters_label = app.t(Key::Manage_Filters);

    widgets::page_header(
        ui,
        &theme,
        app.t(Key::Nav_Builds),
        Some("Installed apps across every Steam library."),
    );
    ui.add_space(12.0);

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
            state.page = 0;
        }
    });
    ui.add_space(8.0);

    // Filter row.
    let mut genres: Vec<String> = app
        .snapshot
        .apps
        .iter()
        .filter_map(|app| app.metadata.as_ref())
        .flat_map(|metadata| metadata.genres.iter().cloned())
        .collect();
    genres.sort();
    genres.dedup();
    let mut years: Vec<i32> = app
        .snapshot
        .apps
        .iter()
        .filter_map(|app| app.metadata.as_ref())
        .filter_map(|metadata| metadata.release_year)
        .collect();
    years.sort_unstable();
    years.dedup();
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(filters_label)
                .size(theme.small_size)
                .color(theme.muted),
        );
        egui::ComboBox::from_id_salt("builds-os")
            .selected_text(state.os.label())
            .show_ui(ui, |ui| {
                for os in OsFilter::ALL {
                    if ui.selectable_value(&mut state.os, os, os.label()).clicked() {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-genre")
            .selected_text(if state.genre.is_empty() {
                "Any genre".to_owned()
            } else {
                state.genre.clone()
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut state.genre, String::new(), "Any genre")
                    .clicked()
                {
                    state.page = 0;
                }
                for genre in &genres {
                    if ui
                        .selectable_value(&mut state.genre, genre.clone(), genre)
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-kind")
            .selected_text(state.kind.label())
            .show_ui(ui, |ui| {
                for kind in KindFilter::ALL {
                    if ui
                        .selectable_value(&mut state.kind, kind, kind.label())
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-year")
            .selected_text(state.year.map_or_else(
                || app.t(Key::Manage_Opt_Any).to_owned(),
                |year| year.to_string(),
            ))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut state.year, None, app.t(Key::Manage_Opt_Any))
                    .clicked()
                {
                    state.page = 0;
                }
                for year in &years {
                    if ui
                        .selectable_value(&mut state.year, Some(*year), year.to_string())
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-price")
            .selected_text(match state.price {
                PriceFilter::Any => app.t(Key::Manage_Opt_Any),
                PriceFilter::Free => app.t(Key::Manage_Opt_Free),
                PriceFilter::Paid => app.t(Key::Manage_Opt_Paid),
            })
            .show_ui(ui, |ui| {
                for (value, label) in [
                    (PriceFilter::Any, Key::Manage_Opt_Any),
                    (PriceFilter::Free, Key::Manage_Opt_Free),
                    (PriceFilter::Paid, Key::Manage_Opt_Paid),
                ] {
                    if ui
                        .selectable_value(&mut state.price, value, app.t(label))
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-content")
            .selected_text(match state.content {
                ContentFilter::Any => app.t(Key::Manage_Opt_Any),
                ContentFilter::HideAdult => app.t(Key::Manage_Content_HideAdult),
                ContentFilter::AdultOnly => app.t(Key::Manage_Content_AdultOnly),
            })
            .show_ui(ui, |ui| {
                for (value, label) in [
                    (ContentFilter::Any, Key::Manage_Opt_Any),
                    (ContentFilter::HideAdult, Key::Manage_Content_HideAdult),
                    (ContentFilter::AdultOnly, Key::Manage_Content_AdultOnly),
                ] {
                    if ui
                        .selectable_value(&mut state.content, value, app.t(label))
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        egui::ComboBox::from_id_salt("builds-sort")
            .selected_text(app.t(state.sort.label()))
            .show_ui(ui, |ui| {
                for sort in Sort::ALL {
                    if ui
                        .selectable_value(&mut state.sort, sort, app.t(sort.label()))
                        .clicked()
                    {
                        state.page = 0;
                    }
                }
            });
        if widgets::link_button(ui, &theme, clear_label).clicked() {
            state.clear();
        }
    });
    ui.add_space(8.0);

    let mut matches: Vec<AppEntry> =
        app.snapshot
            .apps
            .iter()
            .filter(|entry| {
                (!state.installed_only || entry.installed.is_installed())
                    && entry.matches(&app.search)
                    && state.os.matches(entry.metadata.as_ref())
                    && (state.genre.is_empty()
                        || entry.metadata.as_ref().is_some_and(|metadata| {
                            metadata.genres.iter().any(|g| g == &state.genre)
                        }))
                    && state.kind.matches(entry.metadata.as_ref())
                    && state.price.matches(entry.metadata.as_ref())
                    && state.content.matches(entry.metadata.as_ref())
                    && state.year.is_none_or(|year| {
                        entry
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.release_year)
                            == Some(year)
                    })
            })
            .cloned()
            .collect();
    sort_apps(&mut matches, state.sort);

    let total = matches.len();
    let page_size = if state.page_size == 0 {
        total.max(1)
    } else {
        state.page_size
    };
    let pages = total.div_ceil(page_size).max(1);
    state.page = state.page.min(pages - 1);
    let start = state.page * page_size;
    let end = (start + page_size).min(total);

    ui.horizontal(|ui| {
        let current = (state.page + 1).to_string();
        let total_pages = pages.to_string();
        let page_label = app.tf(Key::Manage_PageLabel, &[&current, &total_pages]);
        ui.label(
            RichText::new(format!("{total} apps  ·  {page_label}"))
                .size(theme.small_size + 0.5)
                .color(theme.muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            egui::ComboBox::from_id_salt("builds-page-size")
                .selected_text(if state.page_size == 0 {
                    app.t(Key::Manage_PageSize_All).to_owned()
                } else {
                    state.page_size.to_string()
                })
                .show_ui(ui, |ui| {
                    for (value, label) in [
                        (10, "10".to_owned()),
                        (24, "24".to_owned()),
                        (50, "50".to_owned()),
                        (0, app.t(Key::Manage_PageSize_All).to_owned()),
                    ] {
                        if ui
                            .selectable_value(&mut state.page_size, value, &label)
                            .clicked()
                        {
                            state.page = 0;
                        }
                    }
                });
            if widgets::secondary_button(ui, &theme, "‹").clicked() && state.page > 0 {
                state.page -= 1;
            }
            if widgets::secondary_button(ui, &theme, "›").clicked() && state.page + 1 < pages {
                state.page += 1;
            }
        });
    });
    ui.add_space(8.0);

    if total == 0 {
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
                "Try a different search, or clear the filters.",
            );
        }
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("builds-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for entry in matches[start..end].iter() {
                if app_row(ui, app, entry) {
                    app.pages.manage.selected = Some(entry.id);
                    app.ensure_details(entry.id);
                    app.page = Page::Manage;
                }
                ui.add_space(8.0);
            }
        });
}

fn sort_apps(apps: &mut [AppEntry], sort: Sort) {
    match sort {
        Sort::Name => apps.sort_by_key(|entry| entry.name.to_lowercase()),
        Sort::Release => apps.sort_by(|a, b| {
            release_key(b)
                .cmp(&release_key(a))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        Sort::Metacritic => apps.sort_by(|a, b| {
            metadata_score(b)
                .cmp(&metadata_score(a))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        Sort::Reviews => apps.sort_by(|a, b| {
            metadata_reviews(b)
                .cmp(&metadata_reviews(a))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

fn release_key(entry: &AppEntry) -> i32 {
    entry
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.release_year)
        .unwrap_or(0)
}

fn metadata_score(entry: &AppEntry) -> i32 {
    entry
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.metacritic)
        .unwrap_or(0)
}

fn metadata_reviews(entry: &AppEntry) -> i64 {
    entry
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.reviews)
        .unwrap_or(0)
}

/// Draws one row and returns true when it was clicked.
fn app_row(ui: &mut egui::Ui, app: &mut AppState, entry: &AppEntry) -> bool {
    let theme = app.theme;
    let installed = entry.installed.is_installed();
    let size = entry.installed.size_on_disk();
    let selected = app.pages.manage.selected == Some(entry.id);
    let cover_path = entry.cover_path.clone();
    let texture = app.cover_texture(ui.ctx(), entry.id, cover_path.as_deref());
    let id = entry.id;

    let response = widgets::clickable_card(ui, &theme, id, |ui| {
        ui.horizontal(|ui| {
            cover_thumb(ui, &theme, &entry.name, texture);
            let meta_width = 260.0;
            let name_width = (ui.available_width() - meta_width).max(140.0);
            ui.allocate_ui_with_layout(
                egui::vec2(name_width, 44.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.name)
                                .size(theme.text_size + 0.5)
                                .color(theme.text)
                                .strong(),
                        )
                        .truncate(),
                    );
                    ui.label(
                        RichText::new(meta_line(app, entry))
                            .size(theme.small_size)
                            .color(theme.muted),
                    );
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if installed {
                    widgets::badge(
                        ui,
                        &theme,
                        app.t(Key::Plugin_Status_Installed),
                        Tone::Success,
                    );
                } else {
                    widgets::badge(
                        ui,
                        &theme,
                        app.t(Key::Plugin_Status_NotInstalled),
                        Tone::Muted,
                    );
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
            egui::Stroke::new(1.5, theme.accent),
            egui::StrokeKind::Inside,
        );
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn meta_line(app: &AppState, entry: &AppEntry) -> String {
    let mut parts = vec![app.tf(Key::Common_AppFallback, &[&entry.id.to_string()])];
    if let Some(metadata) = &entry.metadata {
        if let Some(year) = metadata.release_year {
            parts.push(year.to_string());
        }
        if !metadata.genres.is_empty() {
            parts.push(metadata.genres.join(", "));
        }
    } else if let Some(developer) = &entry.developer {
        parts.push(developer.clone());
    }
    parts.join("  ·  ")
}

/// A cover image or a rounded stand-in with the app's initial.
fn cover_thumb(ui: &mut egui::Ui, theme: &Theme, name: &str, texture: Option<egui::TextureHandle>) {
    let size = egui::vec2(72.0, 34.0);
    if let Some(texture) = texture {
        ui.add(egui::Image::new((texture.id(), size)).corner_radius(theme.radius_control));
        return;
    }
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, theme.radius_control, theme.bg);
    ui.painter().rect_stroke(
        rect,
        theme.radius_control,
        egui::Stroke::new(1.0, theme.border),
        egui::StrokeKind::Inside,
    );
    let initial = name
        .chars()
        .find(|c| !c.is_whitespace())
        .map_or('?', |c| c.to_ascii_uppercase());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(16.0),
        theme.accent,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use luatools_core::{AppEntry, AppId, BackendHandle, InstalledState, Settings};

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
    fn renders_searchable_result_rows() {
        let mut app = state();
        let mut cs2 = AppEntry::new(AppId::new(730), "Counter-Strike 2");
        cs2.installed = InstalledState::Installed {
            path: PathBuf::from("/steam/steamapps/common/Counter-Strike 2"),
            size_on_disk: 30_000_000_000,
            build_id: Some("1".to_owned()),
        };
        app.snapshot.apps = vec![cs2, AppEntry::new(AppId::new(570), "Dota 2")];
        let mut page = State::default();
        render(&mut app, &mut page);
        app.search = "dota".to_owned();
        render(&mut app, &mut page);
    }

    #[test]
    fn clear_resets_every_filter() {
        let mut page = State {
            installed_only: true,
            os: OsFilter::Linux,
            genre: "Action".to_owned(),
            kind: KindFilter::Game,
            year: Some(2024),
            price: PriceFilter::Free,
            content: ContentFilter::HideAdult,
            sort: Sort::Reviews,
            page: 4,
            page_size: 10,
        };
        page.clear();
        assert!(!page.installed_only);
        assert_eq!(page.os, OsFilter::Any);
        assert!(page.genre.is_empty());
        assert_eq!(page.kind, KindFilter::Any);
        assert_eq!(page.year, None);
        assert_eq!(page.price, PriceFilter::Any);
        assert_eq!(page.content, ContentFilter::Any);
        assert_eq!(page.sort, Sort::Name);
        assert_eq!(page.page, 0);
        assert_eq!(page.page_size, 24);
    }
    #[test]
    fn metadata_filters_match_type_price_and_content() {
        let metadata = AppMetadata {
            app_type: Some("game".to_owned()),
            is_free: true,
            is_adult: false,
            windows: true,
            ..AppMetadata::default()
        };
        assert!(KindFilter::Game.matches(Some(&metadata)));
        assert!(!KindFilter::Dlc.matches(Some(&metadata)));
        assert!(PriceFilter::Free.matches(Some(&metadata)));
        assert!(!PriceFilter::Paid.matches(Some(&metadata)));
        assert!(ContentFilter::HideAdult.matches(Some(&metadata)));
        assert!(!ContentFilter::AdultOnly.matches(Some(&metadata)));
        assert!(OsFilter::Windows.matches(Some(&metadata)));
        assert!(!OsFilter::Linux.matches(Some(&metadata)));
        assert!(!KindFilter::Game.matches(None));
        assert!(KindFilter::Any.matches(None));
    }
}

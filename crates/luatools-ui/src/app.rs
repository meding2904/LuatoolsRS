//! Window shell: chrome, navigation, routing, and overlays.

use eframe::egui::{self, Align, Layout, Margin, RichText, Stroke};
use luatools_core::{CloseBehavior, Event, Settings, ThemePreference, UpdateState};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::pages::{self, Page};
use crate::state::{AppState, ConfirmDialog};
use crate::theme::{Theme, Tone};
use crate::widgets;

/// Width of the navigation rail.
const NAV_WIDTH: f32 = 208.0;

/// Height of the title bar.
const TITLEBAR_HEIGHT: f32 = 48.0;

/// The application window.
pub struct LuaToolsApp {
    /// Rendered state.
    pub state: AppState,
    events: UnboundedReceiver<Event>,
    applied_theme: ThemePreference,
}

impl LuaToolsApp {
    /// Wires the shell to the backend channels and persisted settings.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: luatools_core::BackendHandle,
        events: UnboundedReceiver<Event>,
        settings: Settings,
    ) -> Self {
        let state = AppState::new(backend, settings.clone());
        state.theme.install(&cc.egui_ctx, state.dark_mode);
        Self {
            applied_theme: settings.theme,
            state,
            events,
        }
    }

    /// Persists the window size and applies the configured close behaviour.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            self.state.settings.window_size = [rect.width(), rect.height()];
        }
        match self.state.settings.close_behavior {
            CloseBehavior::Exit => self.state.save_settings(),
            CloseBehavior::ToTray => {
                // Without a tray implementation the window would be unreachable, so it stays
                // visible until the tray icon lands.
                tracing::info!("close-to-tray requested; keeping the window visible for now");
                self.state.save_settings();
            }
        }
    }

    /// Applies the theme when the preference changed.
    fn sync_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == self.state.settings.theme {
            return;
        }
        self.applied_theme = self.state.settings.theme;
        self.state.dark_mode = match self.applied_theme {
            ThemePreference::Light => false,
            // The system preference is resolved by the windowing layer; until then, dark.
            ThemePreference::System | ThemePreference::Dark => true,
        };
        self.state.theme = if self.state.dark_mode {
            Theme::dark()
        } else {
            Theme::light()
        };
        self.state.theme.install(ctx, self.state.dark_mode);
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let theme = self.state.theme;
        ui.horizontal_centered(|ui| {
            ui.label(
                RichText::new("LuaTools")
                    .size(15.0)
                    .color(theme.text)
                    .strong(),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new("rust")
                    .size(theme.small_size)
                    .color(theme.accent),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(theme.small_size)
                        .color(theme.faint),
                );
                ui.add_space(8.0);
                self.status_pills(ui);
            });
        });
    }

    fn status_pills(&mut self, ui: &mut egui::Ui) {
        let theme = self.state.theme;
        if self.state.is_busy("library") {
            ui.spinner();
        }
        match &self.state.update {
            UpdateState::Available { latest, .. } => {
                if widgets::link_button(ui, &theme, latest).clicked() {
                    self.state.send(luatools_core::Command::InstallUpdate);
                }
            }
            UpdateState::Downloading { progress } => {
                ui.add(egui::ProgressBar::new(*progress).desired_width(90.0));
            }
            UpdateState::ReadyToInstall { .. } => {
                if widgets::primary_button(ui, &theme, "Restart to update").clicked() {
                    self.state.send(luatools_core::Command::Quit);
                }
            }
            UpdateState::Idle
            | UpdateState::Checking
            | UpdateState::UpToDate { .. }
            | UpdateState::Failed { .. } => {}
        }
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        let theme = self.state.theme;
        ui.add_space(8.0);
        for page in Page::NAV {
            let selected = self.state.page == *page;
            if widgets::nav_item(ui, &theme, self.state.t(page.label()), selected).clicked() {
                self.state.page = *page;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} locales", luatools_core::Locale::ALL.len()))
                    .size(theme.small_size)
                    .color(theme.faint),
            );
            widgets::divider(ui, &theme);
            ui.label(
                RichText::new(self.state.settings.language.native_name())
                    .size(theme.small_size)
                    .color(theme.muted),
            );
        });
    }

    fn page(&mut self, ui: &mut egui::Ui) {
        if !self.state.settings.onboarding_completed {
            let mut state = std::mem::take(&mut self.state.pages.onboarding);
            pages::onboarding::show(ui, &mut self.state, &mut state);
            self.state.pages.onboarding = state;
            return;
        }
        let page = self.state.page;
        match page {
            Page::Home => {
                let mut state = std::mem::take(&mut self.state.pages.home);
                pages::home::show(ui, &mut self.state, &mut state);
                self.state.pages.home = state;
            }
            Page::Builds => {
                let mut state = std::mem::take(&mut self.state.pages.builds);
                pages::builds::show(ui, &mut self.state, &mut state);
                self.state.pages.builds = state;
            }
            Page::Downloads => {
                let mut state = std::mem::take(&mut self.state.pages.downloads);
                pages::downloads::show(ui, &mut self.state, &mut state);
                self.state.pages.downloads = state;
            }
            Page::Manage => {
                let mut state = std::mem::take(&mut self.state.pages.manage);
                pages::manage::show(ui, &mut self.state, &mut state);
                self.state.pages.manage = state;
            }
            Page::Settings => {
                let mut state = std::mem::take(&mut self.state.pages.settings);
                pages::settings::show(ui, &mut self.state, &mut state);
                self.state.pages.settings = state;
            }
        }
    }

    fn launch_options_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.state.launch_options.clone() else {
            return;
        };
        let theme = self.state.theme;
        let mut draft = dialog.draft.clone();
        let mut close = false;
        let mut save = false;
        egui::Modal::new(egui::Id::unique("launch-options-dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme.surface)
                    .stroke(Stroke::new(1.0, theme.border))
                    .corner_radius(theme.radius_card)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(
                    RichText::new(dialog.app_name.clone())
                        .size(theme.text_size + 2.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Steam launch options")
                        .size(theme.small_size)
                        .color(theme.muted),
                );
                ui.add_space(10.0);
                ui.add_sized(
                    [ui.available_width(), 72.0],
                    egui::TextEdit::multiline(&mut draft)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("-novid -console"),
                );
                if let Some(error) = &dialog.error {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(error)
                            .size(theme.small_size)
                            .color(theme.danger),
                    );
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::primary_button(ui, &theme, "Save").clicked() {
                        save = true;
                    }
                    if widgets::secondary_button(ui, &theme, "Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if save {
            if let Some(open) = self.state.launch_options.as_mut() {
                open.draft = draft.clone();
            }
            self.state.send(luatools_core::Command::SaveLaunchOptions {
                app_id: dialog.app_id,
                options: draft,
            });
        } else if close {
            self.state.launch_options = None;
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.state.confirm.clone() else {
            return;
        };
        let theme = self.state.theme;
        let mut confirmed = false;
        let mut dismissed = false;
        egui::Modal::new(egui::Id::unique("confirm-dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme.surface)
                    .stroke(Stroke::new(1.0, theme.border))
                    .corner_radius(theme.radius_card)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(
                    RichText::new(&dialog.title)
                        .size(theme.text_size + 2.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(&dialog.body)
                        .size(theme.small_size + 0.5)
                        .color(theme.muted),
                );
                ui.add_space(16.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::danger_button(ui, &theme, &dialog.confirm_label).clicked() {
                        confirmed = true;
                    }
                    if widgets::secondary_button(ui, &theme, "Cancel").clicked() {
                        dismissed = true;
                    }
                });
            });
        if confirmed {
            self.state.send(dialog.action);
            self.state.confirm = None;
        } else if dismissed {
            self.state.confirm = None;
        }
    }
}

impl eframe::App for LuaToolsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.state.drain_events(&mut self.events);
        self.sync_theme(ui.ctx());
        self.handle_close_request(ui.ctx());

        if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            self.state.launch_options = None;
            self.state.confirm = None;
        }

        let theme = self.state.theme;
        egui::Panel::top("titlebar")
            .exact_size(TITLEBAR_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(theme.panel)
                    .inner_margin(Margin::symmetric(16, 0))
                    .stroke(Stroke::new(1.0, theme.border)),
            )
            .show(ui, |ui| self.title_bar(ui));

        egui::Panel::left("nav")
            .exact_size(NAV_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.panel)
                    .inner_margin(Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0, theme.border)),
            )
            .show(ui, |ui| self.nav(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.bg)
                    .inner_margin(Margin::same(24)),
            )
            .show(ui, |ui| self.page(ui));

        self.launch_options_dialog(ui.ctx());
        self.confirm_dialog(ui.ctx());
        widgets::toast_stack(ui, &theme, &mut self.state.toasts);

        // Keep rendering while a task is running so progress animates.
        if !self.state.busy.is_empty() || !self.state.active_downloads().next().is_none() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
}

/// Creates a confirmation dialog for a destructive command.
#[must_use]
pub fn confirm(
    title: &str,
    body: &str,
    confirm_label: &str,
    action: luatools_core::Command,
) -> ConfirmDialog {
    ConfirmDialog {
        title: title.to_owned(),
        body: body.to_owned(),
        confirm_label: confirm_label.to_owned(),
        action,
    }
}

/// Semantic tone used by the shell for status text.
#[must_use]
pub const fn brand_tone() -> Tone {
    Tone::Accent
}

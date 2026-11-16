//! Window shell: chrome, navigation, routing, and overlays.

use eframe::egui::{self, Align, Layout, Margin, RichText, Stroke};
use luatools_core::{CloseBehavior, Event, Settings, ThemePreference, UpdateState};
use luatools_i18n::Key;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::pages::{self, Page};
use crate::state::{AppState, ConfirmDialog};
use crate::theme::{Theme, Tone};
use crate::tray::{Tray, TrayAction};
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
    start_minimized: bool,
    tray: Option<Tray>,
    quitting: bool,
}

impl LuaToolsApp {
    /// Wires the shell to the backend channels and persisted settings.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: luatools_core::BackendHandle,
        events: UnboundedReceiver<Event>,
        settings: Settings,
        start_minimized: bool,
    ) -> Self {
        let state = AppState::new(backend, settings.clone());
        state.theme.install(&cc.egui_ctx, state.dark_mode);
        let mut tray = Tray::new(state.t(Key::Tray_Open), state.t(Key::Tray_Exit));
        if let Some(tray) = tray.as_mut() {
            tray.set_visible(start_minimized);
        }
        Self {
            applied_theme: settings.theme,
            state,
            events,
            start_minimized,
            tray,
            quitting: false,
        }
    }

    /// Persists the window size and applies the configured close behaviour.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.state.settings.remember_window
            && let Some(rect) = ctx.input(|i| i.viewport().inner_rect)
        {
            self.state.settings.window_size = [rect.width(), rect.height()];
        }
        self.state.settings.last_page = Some(self.state.page.id().to_owned());
        self.state.save_settings();
        if !self.quitting
            && self.state.settings.close_behavior == CloseBehavior::ToTray
            && let Some(tray) = self.tray.as_mut()
        {
            tray.set_visible(true);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    /// Polls the tray menu and applies its window action.
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let Some(action) = self.tray.as_ref().and_then(Tray::poll) else {
            return;
        };
        match action {
            TrayAction::Open => {
                if let Some(tray) = self.tray.as_mut() {
                    tray.set_visible(false);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            TrayAction::Exit => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

    /// Turns dropped URLs/text into protocol actions.
    fn handle_dropped_input(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        for file in dropped {
            let text = std::fs::read_to_string(file.path()).ok().or_else(|| {
                file.bytes()
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            });
            let Some(text) = text else {
                continue;
            };
            let text = text.trim();
            if text.starts_with("http://")
                || text.starts_with("https://")
                || text.starts_with("steam://")
                || text.starts_with("luatools://")
            {
                self.state
                    .send(luatools_core::Command::HandleProtocolUrl(text.to_owned()));
            } else if let Some(url) = text
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("URL="))
                .and_then(|line| line.strip_prefix("URL="))
            {
                self.state
                    .send(luatools_core::Command::HandleProtocolUrl(url.to_owned()));
            }
        }
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
                if widgets::primary_button(ui, &theme, "Open update folder").clicked() {
                    self.state.send(luatools_core::Command::OpenUpdateFolder);
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
            if widgets::link_button(ui, &theme, self.state.t(Key::Nav_RestartSteam)).clicked() {
                self.state.confirm = Some(confirm(
                    self.state.t(Key::Manage_RestartSteam_Title),
                    self.state.t(Key::Main_RestartSteam_Ask),
                    self.state.t(Key::Nav_RestartSteam),
                    luatools_core::Command::RestartSteam,
                ));
            }
            ui.label(
                RichText::new(self.state.settings.language.native_name())
                    .size(theme.small_size)
                    .color(theme.muted),
            );
            widgets::divider(ui, &theme);
            match &self.state.auth {
                luatools_core::AuthState::SignedIn { profile } => {
                    let name = profile
                        .display_name
                        .as_deref()
                        .or(profile.email.as_deref())
                        .unwrap_or("Account");
                    ui.label(RichText::new(name).size(theme.small_size).color(theme.text));
                    if widgets::link_button(ui, &theme, self.state.t(Key::Settings_SignOut))
                        .clicked()
                    {
                        self.state.send(luatools_core::Command::SignOut);
                    }
                }
                luatools_core::AuthState::WaitingForBrowser { .. } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        if widgets::link_button(
                            ui,
                            &theme,
                            self.state.t(Key::Nav_WaitingForBrowser),
                        )
                        .clicked()
                        {
                            self.state.send(luatools_core::Command::CancelSignIn);
                        }
                    });
                }
                luatools_core::AuthState::Failed { .. } | luatools_core::AuthState::Guest => {
                    ui.label(
                        RichText::new(self.state.t(Key::Nav_Footer_Guest))
                            .size(theme.small_size)
                            .color(theme.muted),
                    );
                    if widgets::link_button(ui, &theme, self.state.t(Key::Nav_SignInDiscord))
                        .clicked()
                    {
                        self.state.send(luatools_core::Command::SignIn);
                    }
                }
            }
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
            Page::Add => {
                let mut state = std::mem::take(&mut self.state.pages.add);
                pages::add::show(ui, &mut self.state, &mut state);
                self.state.pages.add = state;
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
            Page::Plugin => {
                let mut state = std::mem::take(&mut self.state.pages.plugin);
                pages::plugin::show(ui, &mut self.state, &mut state);
                self.state.pages.plugin = state;
            }
            Page::Settings => {
                let mut state = std::mem::take(&mut self.state.pages.settings);
                pages::settings::show(ui, &mut self.state, &mut state);
                self.state.pages.settings = state;
            }
        }
    }

    fn launch_options_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.state.launch_options.take() else {
            return;
        };
        let theme = self.state.theme;
        let mut close = false;
        let mut save = false;
        let mut restore = false;
        let mut add_entry = false;
        let mut delete_entry = false;

        egui::Modal::new(egui::Id::unique("launch-options-dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme.surface)
                    .stroke(Stroke::new(1.0, theme.border))
                    .corner_radius(theme.radius_card)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(720.0);
                ui.label(
                    RichText::new(&dialog.app_name)
                        .size(theme.text_size + 2.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(self.state.t(Key::Launch_Title))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
                ui.add_space(12.0);

                ui.columns(2, |columns| {
                    let left = &mut columns[0];
                    left.set_max_width(230.0);
                    widgets::section_label(left, &theme, "ENTRIES");
                    left.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .id_salt("launch-entry-list")
                        .max_height(320.0)
                        .show(left, |ui| {
                            for (index, entry) in dialog.entries.iter().enumerate() {
                                let label = if entry.description.trim().is_empty() {
                                    entry.summary()
                                } else {
                                    entry.description.clone()
                                };
                                let selected = dialog.selected == index;
                                let text = if label.trim().is_empty() {
                                    format!("Entry {}", index + 1)
                                } else {
                                    label
                                };
                                if widgets::nav_item(ui, &theme, &text, selected).clicked() {
                                    dialog.selected = index;
                                }
                            }
                        });
                    left.add_space(8.0);
                    left.horizontal(|ui| {
                        if widgets::secondary_button(ui, &theme, self.state.t(Key::Launch_Add))
                            .clicked()
                        {
                            add_entry = true;
                        }
                        if widgets::danger_button(ui, &theme, self.state.t(Key::Launch_Delete))
                            .clicked()
                        {
                            delete_entry = true;
                        }
                    });

                    let right = &mut columns[1];
                    right.set_max_width(440.0);
                    widgets::section_label(right, &theme, "SELECTED ENTRY");
                    right.add_space(6.0);
                    if let Some(entry) = dialog.entries.get_mut(dialog.selected) {
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_Executable),
                            &mut entry.executable,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_Arguments),
                            &mut entry.arguments,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_WorkingDir),
                            &mut entry.working_dir,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_Description),
                            &mut entry.description,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_Type),
                            &mut entry.option_type,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_Os),
                            &mut entry.os_list,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_BetaKey),
                            &mut entry.beta_key,
                        );
                        edit_field(
                            right,
                            &theme,
                            self.state.t(Key::Launch_Field_OwnsDlc),
                            &mut entry.owns_dlc,
                        );
                    } else {
                        right.label(
                            RichText::new("No entries. Add one to describe a Play-button launch.")
                                .color(theme.muted),
                        );
                    }
                });

                ui.add_space(12.0);
                widgets::divider(ui, &theme);
                ui.add_space(10.0);
                widgets::section_label(ui, &theme, &self.state.t(Key::Launch_Title).to_uppercase());
                ui.add_space(4.0);
                ui.add_sized(
                    [ui.available_width(), 58.0],
                    egui::TextEdit::multiline(&mut dialog.local_options)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("-novid -console"),
                );
                ui.label(
                    RichText::new(self.state.t(Key::Launch_SteamRestartNote))
                        .size(theme.small_size)
                        .color(theme.muted),
                );

                if let Some(error) = &dialog.error {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(error)
                            .size(theme.small_size)
                            .color(theme.danger),
                    );
                }

                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::primary_button(ui, &theme, self.state.t(Key::Builds_Action_Save))
                        .clicked()
                    {
                        save = true;
                    }
                    if widgets::secondary_button(
                        ui,
                        &theme,
                        self.state.t(Key::Builds_Action_Cancel),
                    )
                    .clicked()
                    {
                        close = true;
                    }
                    if widgets::link_button(ui, &theme, self.state.t(Key::Launch_Restore)).clicked()
                    {
                        restore = true;
                    }
                });
            });

        if add_entry {
            dialog.entries.push(luatools_core::LaunchOption {
                description: "Play".to_owned(),
                ..luatools_core::LaunchOption::default()
            });
            dialog.selected = dialog.entries.len().saturating_sub(1);
        }
        if delete_entry && !dialog.entries.is_empty() {
            let index = dialog.selected.min(dialog.entries.len() - 1);
            dialog.entries.remove(index);
            dialog.selected = index.min(dialog.entries.len().saturating_sub(1));
        }

        if restore {
            self.state
                .send(luatools_core::Command::RestoreLaunchOptions(dialog.app_id));
            self.state.launch_options = None;
            return;
        }

        if save {
            if dialog
                .entries
                .iter()
                .any(|entry| entry.executable.trim().is_empty())
            {
                dialog.error = Some(self.state.t(Key::Launch_ExecutableRequired).to_owned());
            } else {
                self.state.send(luatools_core::Command::SaveLaunchEntries {
                    app_id: dialog.app_id,
                    entries: dialog.entries.clone(),
                });
                self.state.send(luatools_core::Command::SaveLaunchOptions {
                    app_id: dialog.app_id,
                    options: dialog.local_options.clone(),
                });
            }
        }

        if close {
            self.state.launch_options = None;
        } else {
            self.state.launch_options = Some(dialog);
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
                    if widgets::secondary_button(
                        ui,
                        &theme,
                        self.state.t(Key::Builds_Action_Cancel),
                    )
                    .clicked()
                    {
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
        self.poll_tray(ui.ctx());
        if self.start_minimized {
            self.start_minimized = false;
            if let Some(tray) = self.tray.as_mut() {
                tray.set_visible(true);
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
        if self.state.show_window {
            self.state.show_window = false;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            if let Some(tray) = self.tray.as_mut() {
                tray.set_visible(false);
            }
        }
        self.handle_dropped_input(ui.ctx());
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
        } else if self.tray.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(400));
        }
    }
}

/// One label/text-edit pair in the launch-options editor.
fn edit_field(ui: &mut egui::Ui, theme: &Theme, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [92.0, 20.0],
            egui::Label::new(
                RichText::new(label)
                    .size(theme.small_size)
                    .color(theme.muted),
            ),
        );
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(8, 5)),
        );
    });
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

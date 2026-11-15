//! First-run flow.

use eframe::egui::{self, Align, Layout, RichText, Sense, Stroke};
use luatools_core::{Command, Locale};

use crate::pages::Page;
use crate::state::AppState;
use crate::theme::Theme;
use crate::widgets;

/// Onboarding steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
    /// Welcome and scope.
    #[default]
    Welcome,
    /// Pick a language.
    Language,
    /// Confirm the detected Steam folder.
    SteamFolder,
    /// Summary.
    Done,
}

impl Step {
    /// Every step, in order.
    const ALL: [Self; 4] = [Self::Welcome, Self::Language, Self::SteamFolder, Self::Done];

    /// Zero-based position in the flow.
    const fn index(self) -> usize {
        match self {
            Self::Welcome => 0,
            Self::Language => 1,
            Self::SteamFolder => 2,
            Self::Done => 3,
        }
    }

    /// The next step, or itself at the end.
    const fn next(self) -> Self {
        match self {
            Self::Welcome => Self::Language,
            Self::Language => Self::SteamFolder,
            Self::SteamFolder | Self::Done => Self::Done,
        }
    }

    /// The previous step, or itself at the start.
    const fn previous(self) -> Self {
        match self {
            Self::Welcome | Self::Language => Self::Welcome,
            Self::SteamFolder => Self::Language,
            Self::Done => Self::SteamFolder,
        }
    }

    /// Section label shown above the step body.
    const fn title(self) -> &'static str {
        match self {
            Self::Welcome => "WELCOME",
            Self::Language => "LANGUAGE",
            Self::SteamFolder => "STEAM FOLDER",
            Self::Done => "READY",
        }
    }
}

/// Onboarding transient state.
#[derive(Debug, Default)]
pub struct State {
    /// Current step.
    pub step: Step,
    /// Pending native folder picker for the Steam root.
    pub steam_picker: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
}

/// Renders onboarding full-screen. The shell decides when to show it.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    ui.add_space(32.0);

    // A centred column rather than a full-width card: the flow reads like a dialog.
    let available = ui.available_width();
    let content_width = available.min(560.0);
    let inset = ((available - content_width) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(inset);
        ui.vertical(|ui| {
            ui.set_width(content_width);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("LuaTools")
                        .size(30.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Your Steam library, without the noise.")
                        .size(theme.text_size)
                        .color(theme.muted),
                );
            });
            ui.add_space(20.0);
            step_dots(ui, &theme, state.step);
            ui.add_space(12.0);

            widgets::card(ui, &theme, |ui| {
                widgets::section_label(ui, &theme, state.step.title());
                ui.add_space(8.0);
                step_body(ui, app, state, &theme);
            });

            ui.add_space(16.0);
            navigation(ui, app, state, &theme);
        });
    });
}

/// The step-specific content.
fn step_body(ui: &mut egui::Ui, app: &mut AppState, state: &mut State, theme: &Theme) {
    match state.step {
        Step::Welcome => {
            ui.label(
                RichText::new(
                    "LuaTools scans your Steam libraries, edits launch options, and downloads \
                     depot content for your own account.",
                )
                .size(theme.text_size)
                .color(theme.text),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "This short tour picks an interface language and confirms where Steam is \
                     installed. Nothing is changed without your confirmation.",
                )
                .size(theme.text_size)
                .color(theme.muted),
            );
        }
        Step::Language => {
            let mut selected = app.settings.language;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Interface language")
                        .size(theme.text_size)
                        .color(theme.text),
                );
                egui::ComboBox::from_id_salt("onboarding-language")
                    .selected_text(selected.native_name())
                    .show_ui(ui, |ui| {
                        for locale in Locale::ALL {
                            ui.selectable_value(&mut selected, *locale, locale.native_name());
                        }
                    });
            });
            if selected != app.settings.language {
                app.set_locale(selected);
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new("You can change this later in Settings, and it applies everywhere.")
                    .size(theme.small_size)
                    .color(theme.muted),
            );
        }
        Step::SteamFolder => {
            match app.snapshot.steam_root.as_ref() {
                Some(root) => {
                    widgets::field_row(ui, theme, "Steam root", &root.display().to_string())
                }
                None => {
                    ui.label(
                        RichText::new("Steam was not found in the usual locations.")
                            .size(theme.text_size)
                            .color(theme.warning),
                    );
                }
            }
            widgets::field_row(
                ui,
                theme,
                "Library apps",
                &app.snapshot.apps.len().to_string(),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if app.is_busy("library") {
                    ui.spinner();
                    ui.label(
                        RichText::new("Scanning Steam libraries…")
                            .size(theme.small_size + 0.5)
                            .color(theme.muted),
                    );
                } else if widgets::secondary_button(ui, theme, "Scan now").clicked() {
                    app.send(Command::RefreshLibrary);
                }
                if widgets::secondary_button(ui, theme, "Choose folder…").clicked() {
                    state.steam_picker = Some(crate::dialogs::pick_folder(
                        app.settings
                            .steam_path
                            .clone()
                            .or_else(|| app.snapshot.steam_root.clone()),
                    ));
                }
            });
            if let Some(result) = crate::dialogs::poll(&mut state.steam_picker)
                && let Some(path) = result
            {
                app.settings.steam_path = Some(path);
                app.save_settings();
                app.send(Command::RefreshLibrary);
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "LuaTools reads Steam's library folders and app manifests. Your install is \
                     never modified by a scan.",
                )
                .size(theme.small_size)
                .color(theme.muted),
            );
        }
        Step::Done => {
            ui.label(
                RichText::new("Everything is ready.")
                    .size(theme.text_size + 2.0)
                    .color(theme.text)
                    .strong(),
            );
            ui.add_space(10.0);
            widgets::field_row(ui, theme, "Language", app.settings.language.native_name());
            widgets::field_row(ui, theme, "Steam root", &steam_root_label(app));
            widgets::field_row(
                ui,
                theme,
                "Library apps",
                &app.snapshot.apps.len().to_string(),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new("You can revisit all of these choices from Settings.")
                    .size(theme.small_size)
                    .color(theme.muted),
            );
        }
    }
}

/// Back / Continue / Finish controls.
fn navigation(ui: &mut egui::Ui, app: &mut AppState, state: &mut State, theme: &Theme) {
    let mut back = false;
    let mut advance = false;

    ui.horizontal(|ui| {
        if state.step != Step::Welcome && widgets::secondary_button(ui, theme, "Back").clicked() {
            back = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if state.step == Step::Done {
                "Finish"
            } else {
                "Continue"
            };
            if widgets::primary_button(ui, theme, label).clicked() {
                advance = true;
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "Step {} of {}",
                    state.step.index() + 1,
                    Step::ALL.len()
                ))
                .size(theme.small_size)
                .color(theme.muted),
            );
        });
    });

    if back {
        state.step = state.step.previous();
    }
    if advance {
        if state.step == Step::Done {
            app.settings.onboarding_completed = true;
            app.save_settings();
            app.page = Page::Home;
        } else {
            state.step = state.step.next();
        }
    }
}

/// Four dots with the current position filled.
fn step_dots(ui: &mut egui::Ui, theme: &Theme, current: Step) {
    let diameter = 7.0;
    let gap = 8.0;
    let count = Step::ALL.len() as f32;
    let total = count * diameter + (count - 1.0) * gap;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), diameter + 6.0),
        Sense::hover(),
    );

    let mut left = rect.center().x - total * 0.5;
    for step in Step::ALL {
        let center = egui::pos2(left + diameter * 0.5, rect.center().y);
        if step.index() <= current.index() {
            ui.painter()
                .circle_filled(center, diameter * 0.5, theme.accent);
        } else {
            ui.painter().circle_stroke(
                center,
                diameter * 0.5,
                Stroke::new(1.0, theme.border_strong),
            );
        }
        left += diameter + gap;
    }
}

/// Steam root for the summary step.
fn steam_root_label(app: &AppState) -> String {
    app.snapshot.steam_root.as_ref().map_or_else(
        || "Not detected".to_owned(),
        |root| root.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use luatools_core::{BackendHandle, Settings};
    use std::path::PathBuf;

    #[test]
    fn onboarding_renders_every_step() {
        let mut app = AppState::new(BackendHandle::disconnected(), Settings::default());
        app.snapshot.steam_root = Some(PathBuf::from("/steam"));
        let ctx = egui::Context::default();
        for step in Step::ALL {
            let mut state = State {
                step,
                ..State::default()
            };
            let mut output = ctx.run_ui(Default::default(), |ui| show(ui, &mut app, &mut state));
            // No renderer consumes the texture delta in a headless pass.
            output.textures_delta.clear();
        }
    }

    #[test]
    fn steps_move_in_both_directions() {
        assert_eq!(Step::Welcome.next(), Step::Language);
        assert_eq!(Step::Language.next(), Step::SteamFolder);
        assert_eq!(Step::SteamFolder.next(), Step::Done);
        assert_eq!(Step::Done.next(), Step::Done);

        assert_eq!(Step::Done.previous(), Step::SteamFolder);
        assert_eq!(Step::SteamFolder.previous(), Step::Language);
        assert_eq!(Step::Language.previous(), Step::Welcome);
        assert_eq!(Step::Welcome.previous(), Step::Welcome);
    }
}

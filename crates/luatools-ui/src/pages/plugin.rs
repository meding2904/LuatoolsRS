//! Steam store plugin: installation status and management.

use eframe::egui::{Align, Layout, RichText};
use luatools_core::{Command, PluginStatus};
use luatools_i18n::Key;

use crate::state::AppState;
use crate::theme::Tone;
use crate::widgets;

/// Plugin page transient state.
#[derive(Debug, Default)]
pub struct State {}

/// Renders the plugin page.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, _state: &mut State) {
    let theme = app.theme;
    widgets::page_header(
        ui,
        &theme,
        app.t(Key::Plugin_Title),
        Some(app.t(Key::Plugin_Subtitle)),
    );
    ui.add_space(16.0);

    let mut install = false;
    let mut uninstall = false;
    let mut check = false;

    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(app.t(Key::Plugin_CardTitle))
                    .size(theme.text_size + 1.0)
                    .color(theme.text)
                    .strong(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                match &app.plugin {
                    PluginStatus::Installed { .. } => {
                        widgets::badge(
                            ui,
                            &theme,
                            app.t(Key::Plugin_Status_Installed),
                            Tone::Success,
                        );
                    }
                    PluginStatus::UpdateAvailable { .. } => {
                        widgets::badge(
                            ui,
                            &theme,
                            app.t(Key::Plugin_Badge_UpdateAvailable),
                            Tone::Warning,
                        );
                    }
                    PluginStatus::Working { .. } => {
                        ui.spinner();
                    }
                    PluginStatus::Failed { .. } | PluginStatus::NotInstalled => {
                        widgets::badge(
                            ui,
                            &theme,
                            app.t(Key::Plugin_Status_NotInstalled),
                            Tone::Muted,
                        );
                    }
                }
            });
        });
        ui.add_space(10.0);
        widgets::divider(ui, &theme);
        ui.add_space(10.0);

        match &app.plugin {
            PluginStatus::Installed { version } => {
                widgets::field_row(ui, &theme, app.t(Key::Plugin_Row_InstalledVersion), version);
                widgets::field_row(
                    ui,
                    &theme,
                    app.t(Key::Plugin_Row_LatestVersion),
                    app.t(Key::Plugin_Status_UpToDate),
                );
            }
            PluginStatus::UpdateAvailable {
                installed,
                available,
            } => {
                widgets::field_row(
                    ui,
                    &theme,
                    app.t(Key::Plugin_Row_InstalledVersion),
                    installed,
                );
                widgets::field_row(ui, &theme, app.t(Key::Plugin_Row_LatestVersion), available);
            }
            PluginStatus::Working { step } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(step).color(theme.muted));
                });
            }
            PluginStatus::Failed { reason } => {
                ui.label(RichText::new(reason).color(theme.danger));
            }
            PluginStatus::NotInstalled => {
                widgets::field_row(
                    ui,
                    &theme,
                    app.t(Key::Plugin_Row_InstalledVersion),
                    app.t(Key::Plugin_Status_NotInstalled),
                );
                widgets::field_row(
                    ui,
                    &theme,
                    app.t(Key::Plugin_Row_Frontend),
                    app.t(Key::Plugin_Version_Unknown),
                );
                widgets::field_row(
                    ui,
                    &theme,
                    app.t(Key::Plugin_Row_Loader),
                    app.t(Key::Plugin_Version_Unknown),
                );
            }
        }

        ui.add_space(12.0);
        widgets::divider(ui, &theme);
        ui.add_space(10.0);
        ui.label(
            RichText::new(app.t(Key::Plugin_Millennium_Coexisting))
                .size(theme.small_size)
                .color(theme.muted),
        );
        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            let label = match &app.plugin {
                PluginStatus::Installed { .. } => Key::Plugin_Btn_Reinstall,
                PluginStatus::UpdateAvailable { .. } => Key::Plugin_Btn_Update,
                PluginStatus::Working { .. } => Key::Plugin_Checking,
                PluginStatus::Failed { .. } | PluginStatus::NotInstalled => Key::Plugin_Btn_Install,
            };
            if widgets::primary_button(ui, &theme, app.t(label)).clicked() {
                install = true;
            }
            if !matches!(
                app.plugin,
                PluginStatus::NotInstalled | PluginStatus::Working { .. }
            ) && widgets::danger_button(ui, &theme, app.t(Key::Plugin_Btn_Uninstall)).clicked()
            {
                uninstall = true;
            }
            if widgets::link_button(ui, &theme, app.t(Key::Plugin_Btn_CheckForUpdates)).clicked() {
                check = true;
            }
        });
        if !cfg!(windows) {
            ui.add_space(8.0);
            ui.label(
                RichText::new("The Steam plugin loader is Windows-only; other platforms report it as unavailable.")
                    .size(theme.small_size)
                    .color(theme.muted),
            );
        }
    });

    ui.add_space(12.0);
    widgets::card(ui, &theme, |ui| {
        widgets::section_label(ui, &theme, &app.t(Key::Plugin_About_Header).to_uppercase());
        ui.add_space(8.0);
        for feature in [
            Key::Plugin_Feature_StoreButton,
            Key::Plugin_Feature_AutoUpdate,
            Key::Plugin_Feature_Millennium,
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new("•").color(theme.accent));
                ui.label(
                    RichText::new(app.t(feature))
                        .size(theme.small_size + 0.5)
                        .color(theme.text),
                );
            });
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new(app.t(Key::Plugin_Footer))
                .size(theme.small_size)
                .color(theme.faint),
        );
    });

    if install {
        app.send(Command::InstallPlugin);
    }
    if uninstall {
        app.send(Command::UninstallPlugin);
    }
    if check {
        app.send(Command::CheckPluginStatus);
    }
}

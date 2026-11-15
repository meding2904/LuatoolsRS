//! Download queue and history.

use eframe::egui::{self, Align, Layout, RichText};
use luatools_core::{ByteFormat, Command, DownloadKind, DownloadSnapshot, DownloadStatus};
use luatools_i18n::Key;

use crate::state::AppState;
use crate::theme::{Theme, Tone};
use crate::widgets;

/// Which list the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// Jobs that are not finished, including paused ones.
    #[default]
    Active,
    /// Finished jobs.
    History,
}

/// Downloads page transient state.
#[derive(Debug, Default)]
pub struct State {
    /// Selected list.
    pub tab: Tab,
}

/// Renders the queue and the finished-job history.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, state: &mut State) {
    let theme = app.theme;
    let active_count = app
        .downloads
        .iter()
        .filter(|row| !row.status.is_finished())
        .count();
    let history_count = app
        .downloads
        .iter()
        .filter(|row| row.status.is_finished())
        .count();
    let subtitle = format!("{active_count} active · {history_count} finished");
    widgets::page_header(ui, &theme, app.t(Key::Downloads_Title), Some(&subtitle));
    ui.add_space(12.0);

    let mut confirm_clear = false;
    ui.horizontal(|ui| {
        if widgets::chip(ui, &theme, "Active", state.tab == Tab::Active).clicked() {
            state.tab = Tab::Active;
        }
        if widgets::chip(ui, &theme, "History", state.tab == Tab::History).clicked() {
            state.tab = Tab::History;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if state.tab == Tab::History
                && history_count > 0
                && widgets::secondary_button(ui, &theme, app.t(Key::Downloads_Action_ClearHistory))
                    .clicked()
            {
                confirm_clear = true;
            }
        });
    });
    if confirm_clear {
        let removed = history_count.to_string();
        let body = app.tf(Key::Downloads_ClearHistory_Confirm, &[removed.as_str()]);
        app.confirm = Some(crate::app::confirm(
            "Clear download history?",
            &body,
            app.t(Key::Downloads_Action_ClearHistory),
            Command::ClearDownloadHistory,
        ));
    }
    ui.add_space(12.0);

    match state.tab {
        Tab::Active => {
            let rows = sorted_open(&app.downloads);
            if rows.is_empty() {
                widgets::empty_state(
                    ui,
                    &theme,
                    "No active downloads",
                    "Queue a depot or file to follow its progress here.",
                );
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in rows {
                    job_card(ui, app, row);
                }
            });
        }
        Tab::History => {
            let rows = sorted_history(&app.downloads);
            if rows.is_empty() {
                widgets::empty_state(
                    ui,
                    &theme,
                    "No finished downloads",
                    "Completed, failed, and canceled jobs appear here.",
                );
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in rows {
                    history_row(ui, app, row);
                }
            });
        }
    }
}

/// Unfinished jobs, oldest queue time first.
fn sorted_open(downloads: &[DownloadSnapshot]) -> Vec<&DownloadSnapshot> {
    let mut rows: Vec<_> = downloads
        .iter()
        .filter(|row| !row.status.is_finished())
        .collect();
    rows.sort_by(|a, b| a.queued_at.cmp(&b.queued_at).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// Finished jobs, most recently updated first.
fn sorted_history(downloads: &[DownloadSnapshot]) -> Vec<&DownloadSnapshot> {
    let mut rows: Vec<_> = downloads
        .iter()
        .filter(|row| row.status.is_finished())
        .collect();
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.queued_at.cmp(&a.queued_at))
            .then_with(|| b.id.cmp(&a.id))
    });
    rows
}

/// Localized status text.
fn status_label(app: &AppState, status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Queued => app.t(Key::Downloads_Status_Queued),
        DownloadStatus::Preparing => "Preparing",
        DownloadStatus::Downloading => app.t(Key::Downloads_Status_Downloading),
        DownloadStatus::Paused => app.t(Key::Downloads_Status_Paused),
        DownloadStatus::Verifying => app.t(Key::Downloads_Status_Verifying),
        DownloadStatus::Completed => app.t(Key::Downloads_Status_Completed),
        DownloadStatus::Failed => app.t(Key::Downloads_Status_Failed),
        DownloadStatus::Canceled => app.t(Key::Downloads_Status_Cancelled),
    }
}

/// Badge tone for a lifecycle state.
const fn status_tone(status: DownloadStatus) -> Tone {
    match status {
        DownloadStatus::Queued | DownloadStatus::Canceled => Tone::Muted,
        DownloadStatus::Preparing | DownloadStatus::Downloading | DownloadStatus::Verifying => {
            Tone::Accent
        }
        DownloadStatus::Paused => Tone::Warning,
        DownloadStatus::Completed => Tone::Success,
        DownloadStatus::Failed => Tone::Danger,
    }
}

/// App id and job kind shown under the title.
fn subtitle(row: &DownloadSnapshot) -> String {
    let kind = match row.request.kind {
        DownloadKind::Depot => match row.request.depots.len() {
            1 => "1 depot".to_owned(),
            count => format!("{count} depots"),
        },
        DownloadKind::File => "File".to_owned(),
    };
    format!("App {} · {kind}", row.request.app_id)
}

/// Size shown in history rows.
fn size_label(row: &DownloadSnapshot) -> String {
    ByteFormat::new(row.total_bytes.unwrap_or(row.received_bytes)).to_string()
}

/// Progress, rate, and ETA in one line.
fn metrics_text(row: &DownloadSnapshot) -> String {
    let mut text = row.progress_label();
    if row.bytes_per_second > 0 {
        text.push_str("  ·  ");
        text.push_str(&ByteFormat::new(row.bytes_per_second).per_second());
    }
    if let Some(seconds) = row.eta_seconds {
        text.push_str("  ·  ETA ");
        text.push_str(&format_eta(seconds));
    }
    text
}

/// Compact remaining-time string.
fn format_eta(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// One live queue card.
fn job_card(ui: &mut egui::Ui, app: &AppState, row: &DownloadSnapshot) {
    let theme = app.theme;
    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&row.request.app_name)
                        .size(theme.text_size + 1.0)
                        .color(theme.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(subtitle(row))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                widgets::badge(
                    ui,
                    &theme,
                    status_label(app, row.status),
                    status_tone(row.status),
                );
            });
        });

        ui.add_space(8.0);
        widgets::progress_bar(ui, &theme, row.fraction());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(metrics_text(row))
                    .size(theme.small_size)
                    .color(theme.muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                job_actions(ui, app, row, &theme);
            });
        });

        if let Some(error) = &row.error {
            ui.add_space(4.0);
            ui.label(
                RichText::new(error)
                    .size(theme.small_size)
                    .color(theme.danger),
            );
        }
    });
    ui.add_space(8.0);
}

/// State-dependent actions for one live job.
fn job_actions(ui: &mut egui::Ui, app: &AppState, row: &DownloadSnapshot, theme: &Theme) {
    if row.status.is_finished() {
        if widgets::danger_button(ui, theme, app.t(Key::Downloads_Action_Remove)).clicked() {
            app.send(Command::RemoveDownload(row.id));
        }
    } else if widgets::secondary_button(ui, theme, app.t(Key::Downloads_Action_Cancel)).clicked() {
        app.send(Command::CancelDownload(row.id));
    }
    if row.status.can_retry()
        && widgets::secondary_button(ui, theme, app.t(Key::Downloads_Action_Retry)).clicked()
    {
        app.send(Command::RetryDownload(row.id));
    }
    if row.status.can_resume()
        && widgets::primary_button(ui, theme, app.t(Key::Downloads_Action_Resume)).clicked()
    {
        app.send(Command::ResumeDownload(row.id));
    }
    if row.status.can_pause()
        && widgets::secondary_button(ui, theme, app.t(Key::Downloads_Action_Pause)).clicked()
    {
        app.send(Command::PauseDownload(row.id));
    }
}

/// One compact finished-job row.
fn history_row(ui: &mut egui::Ui, app: &AppState, row: &DownloadSnapshot) {
    let theme = app.theme;
    widgets::card(ui, &theme, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&row.request.app_name).color(theme.text));
                ui.label(
                    RichText::new(subtitle(row))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::danger_button(ui, &theme, app.t(Key::Downloads_Action_Remove)).clicked()
                {
                    app.send(Command::RemoveDownload(row.id));
                }
                ui.label(
                    RichText::new(size_label(row))
                        .size(theme.small_size)
                        .color(theme.muted),
                );
                widgets::badge(
                    ui,
                    &theme,
                    status_label(app, row.status),
                    status_tone(row.status),
                );
            });
        });
    });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use luatools_core::{AppId, BackendHandle, DownloadId, DownloadRequest, Settings};

    fn request(name: &str) -> DownloadRequest {
        DownloadRequest {
            app_id: AppId::new(730),
            app_name: name.to_owned(),
            kind: DownloadKind::Depot,
            depots: Vec::new(),
            url: None,
            destination: "/tmp/luatools".into(),
            verify: false,
        }
    }

    fn snapshot(id: u64, name: &str, queued_at: DateTime<Utc>) -> DownloadSnapshot {
        DownloadSnapshot::queued(DownloadId::new(id), request(name), queued_at)
    }

    fn test_app(downloads: Vec<DownloadSnapshot>) -> AppState {
        let mut app = AppState::new(BackendHandle::disconnected(), Settings::default());
        app.downloads = downloads;
        app
    }

    fn render(app: &mut AppState, state: &mut State) {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| show(ui, app, state));
        // No renderer consumes the texture delta in a headless pass.
        output.textures_delta.clear();
    }

    #[test]
    fn renders_one_active_and_one_finished_job() {
        let now = Utc::now();
        let mut active = snapshot(1, "Counter-Strike 2", now);
        active.status = DownloadStatus::Downloading;
        active.total_bytes = Some(1_000_000);
        active.received_bytes = 250_000;
        active.bytes_per_second = 4_200_000;
        active.eta_seconds = Some(42);

        let mut done = snapshot(2, "Dota 2", now - Duration::minutes(5));
        done.status = DownloadStatus::Completed;
        done.updated_at = now;
        done.total_bytes = Some(2_000_000_000);
        done.received_bytes = 2_000_000_000;

        let mut app = test_app(vec![done, active]);
        let mut state = State::default();
        render(&mut app, &mut state);

        state.tab = Tab::History;
        render(&mut app, &mut state);
    }

    #[test]
    fn active_sorts_by_queue_time_and_history_newest_first() {
        let now = Utc::now();
        let mut paused = snapshot(1, "paused", now - Duration::minutes(10));
        paused.status = DownloadStatus::Paused;
        let queued = snapshot(2, "queued", now - Duration::minutes(1));

        let mut finished_old = snapshot(3, "finished old", now - Duration::hours(2));
        finished_old.status = DownloadStatus::Completed;
        finished_old.updated_at = now - Duration::hours(2);
        let mut finished_new = snapshot(4, "finished new", now - Duration::hours(3));
        finished_new.status = DownloadStatus::Failed;
        finished_new.updated_at = now - Duration::minutes(2);

        let downloads = vec![finished_old, queued, finished_new, paused];
        let open: Vec<_> = sorted_open(&downloads)
            .iter()
            .map(|row| row.id.get())
            .collect();
        assert_eq!(open, vec![1, 2], "paused jobs stay in the active list");

        let history: Vec<_> = sorted_history(&downloads)
            .iter()
            .map(|row| row.id.get())
            .collect();
        assert_eq!(history, vec![4, 3]);
    }

    #[test]
    fn eta_uses_the_smallest_useful_unit() {
        assert_eq!(format_eta(9), "9s");
        assert_eq!(format_eta(75), "1m 15s");
        assert_eq!(format_eta(3_725), "1h 02m");
    }
}

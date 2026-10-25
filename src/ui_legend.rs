//! Extension legend pane (top-right).

use eframe::egui;
use crate::app::AppState;

pub fn show(ui: &mut egui::Ui, _state: &mut AppState) {
    ui.heading("Legend");
}

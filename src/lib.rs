pub mod actions;
pub mod app;
pub mod model;
pub mod scan;
pub mod treemap;
pub mod ui_legend;
pub mod ui_tree;
pub mod ui_treemap;

pub fn run() -> eframe::Result {
    eframe::run_native(
        "dirstat",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app::AppState::new()))),
    )
}

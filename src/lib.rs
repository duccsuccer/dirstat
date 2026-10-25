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
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1400.0, 860.0]),
            ..Default::default()
        },
        Box::new(|cc| {
            app::apply_theme(&cc.egui_ctx);
            Ok(Box::new(app::AppState::new()))
        }),
    )
}

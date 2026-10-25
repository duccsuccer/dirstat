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
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            app::apply_theme(&cc.egui_ctx);
            let mut state = app::AppState::new();
            if let Ok(RawWindowHandle::Win32(handle)) = cc.window_handle().map(|h| h.as_raw()) {
                state.hwnd = handle.hwnd.get();
            }
            Ok(Box::new(state))
        }),
    )
}

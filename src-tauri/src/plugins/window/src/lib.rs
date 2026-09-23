use tauri::{
    Runtime, generate_handler,
    plugin::{Builder, TauriPlugin},
};

mod commands;

pub use commands::*;

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("custom-window")
        .invoke_handler(generate_handler![
            commands::show_window,
            commands::hide_window,
            commands::set_always_on_top,
            commands::set_taskbar_visibility,
            commands::show_window_label,
            commands::hide_window_label,
            commands::is_window_visible,
        ])
        .build()
}

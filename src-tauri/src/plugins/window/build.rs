const COMMANDS: &[&str] = &[
    "show_window",
    "hide_window",
    "set_always_on_top",
    "set_taskbar_visibility",
    "show_window_label",
    "hide_window_label",
    "is_window_visible",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}

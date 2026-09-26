use tauri::{AppHandle, Runtime, WebviewWindow, command};

/// Linux 没有强制置顶的保持线程，这里是空实现（保持与 Windows 同名 API）
pub fn stop_topmost_keep_alive(_label: &str) {}

/// Linux 不需要单独切换 webview：WebKitGTK 的视图跟着窗口一起显示 / 隐藏。
pub fn set_webview_visible<R: Runtime>(_window: &WebviewWindow<R>, _visible: bool) {}

#[command]
pub async fn show_window<R: Runtime>(_app_handle: AppHandle<R>, window: WebviewWindow<R>) {
    let _ = window.show();
    let _ = window.unminimize();

    set_webview_visible(&window, true);

    let _ = window.set_focus();
}

#[command]
pub async fn hide_window<R: Runtime>(_app_handle: AppHandle<R>, window: WebviewWindow<R>) {
    let _ = window.hide();

    set_webview_visible(&window, false);
}

#[command]
pub async fn set_always_on_top<R: Runtime>(
    _app_handle: AppHandle<R>,
    window: WebviewWindow<R>,
    always_on_top: bool,
) {
    if always_on_top {
        let _ = window.set_always_on_bottom(false);
        let _ = window.set_always_on_top(true);
    } else {
        let _ = window.set_always_on_top(false);
        let _ = window.set_always_on_bottom(true);
    }
}

#[command]
pub async fn set_taskbar_visibility<R: Runtime>(window: WebviewWindow<R>, visible: bool) {
    let _ = window.set_skip_taskbar(!visible);
}

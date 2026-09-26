use tauri::{AppHandle, Runtime, WebviewWindow, command};

/// 保留跨平台窗口销毁接口；Windows 的置顶状态由系统保持，无后台线程需要停止。
pub fn stop_topmost_keep_alive(_label: &str) {}

/// 让窗口里的 webview 跟着窗口一起显示 / 隐藏。
///
/// `WebviewWindow::show()` / `hide()` 只操作原生窗口，不会下发 `WebviewMessage::Show` /
/// `Hide`，所以 WebView2 那边（控制器与页面的可见性）完全不知道这个窗口被藏过。
/// 对方猫窗口出现过的故障是：页面被 Chromium 冻结在最后一帧、收不到鼠标事件、连自己
/// 的显示开关都点不动；微软给的唤醒口径是「恢复 bounds → 把可见性设回 TRUE → 恢复
/// 焦点 → 仍然不行才 Reload」。窗口切换时让 webview 跟着切一次，才会形成一次真正的
/// 可见性翻转去唤醒它。
pub fn set_webview_visible<R: Runtime>(window: &WebviewWindow<R>, visible: bool) {
    let webview = window.as_ref();

    let _ = if visible {
        webview.show()
    } else {
        webview.hide()
    };
}

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
    // TOPMOST 是持久的窗口属性，不需要每帧重设。多个窗口每 16ms 重设会
    // 反复争抢置顶层内的 Z 序，现场采样显示三个保持线程几乎都耗在 SetWindowPos。
    // 交由 Tauri 在事件循环中设置，避免后台线程直接操作 HWND 及关闭/重开竞态。
    let _ = window.set_always_on_top(always_on_top);
}

#[command]
pub async fn set_taskbar_visibility<R: Runtime>(window: WebviewWindow<R>, visible: bool) {
    let _ = window.set_skip_taskbar(!visible);
}

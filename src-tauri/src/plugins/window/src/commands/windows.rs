use tauri::{AppHandle, Runtime, WebviewWindow, command};

/// 保留跨平台窗口销毁接口；Windows 的置顶状态由系统保持，无后台线程需要停止。
pub fn stop_topmost_keep_alive(_label: &str) {}

#[command]
pub async fn show_window<R: Runtime>(_app_handle: AppHandle<R>, window: WebviewWindow<R>) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

#[command]
pub async fn hide_window<R: Runtime>(_app_handle: AppHandle<R>, window: WebviewWindow<R>) {
    let _ = window.hide();
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

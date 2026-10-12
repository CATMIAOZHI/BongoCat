use tauri::{AppHandle, Manager, Runtime, async_runtime::spawn, command};

pub static MAIN_WINDOW_LABEL: &str = "main";
pub static PREFERENCE_WINDOW_LABEL: &str = "preference";
pub static REMOTE_CAT_WINDOW_LABEL: &str = "remote-cat";
pub static CHAT_WINDOW_LABEL: &str = "chat";

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(target_os = "linux")]
pub use linux::*;

pub fn show_main_window(app_handle: &AppHandle) {
    show_window_by_label(app_handle, MAIN_WINDOW_LABEL);
}

pub fn show_preference_window(app_handle: &AppHandle) {
    show_window_by_label(app_handle, PREFERENCE_WINDOW_LABEL);
}

/// 按窗口 label 显示窗口，供任意窗口切换其它窗口（例如主窗口显示对方猫/聊天窗口）
#[command]
pub async fn show_window_label<R: Runtime>(
    app_handle: AppHandle<R>,
    label: String,
    focus: Option<bool>,
) -> Result<(), String> {
    let Some(window) = app_handle.get_webview_window(&label) else {
        return Err(format!("window not found: {label}"));
    };

    let focus = focus.unwrap_or(false);

    if label == MAIN_WINDOW_LABEL {
        show_window(app_handle.clone(), window).await;
    } else {
        let _ = window.show();
        let _ = window.unminimize();

        // 只把原生窗口显示出来不够：`WebviewWindow::show()` 不会下发 `WebviewMessage::Show`，
        // 被冻结过的页面（对方猫窗口遇到的那种）靠这一步是醒不过来的。
        set_webview_visible(&window, true);

        // 隐藏再显示会让任务栏按钮回来（见 `restore_taskbar_visibility`）
        #[cfg(target_os = "windows")]
        windows::restore_taskbar_visibility(&window);

        if focus {
            let _ = window.set_focus();
        }
    }

    Ok(())
}

/// 按窗口 label 隐藏窗口
#[command]
pub async fn hide_window_label<R: Runtime>(
    app_handle: AppHandle<R>,
    label: String,
) -> Result<(), String> {
    let Some(window) = app_handle.get_webview_window(&label) else {
        return Err(format!("window not found: {label}"));
    };

    if label == MAIN_WINDOW_LABEL {
        hide_window(app_handle.clone(), window).await;
    } else {
        let _ = window.hide();

        // 和显示对称：也让 webview 切一次，下次显示时那次可见性翻转才是真的变化
        set_webview_visible(&window, false);
    }

    Ok(())
}

/// 读取窗口当前是否可见
#[command]
pub async fn is_window_visible<R: Runtime>(
    app_handle: AppHandle<R>,
    label: String,
) -> Result<bool, String> {
    let Some(window) = app_handle.get_webview_window(&label) else {
        return Err(format!("window not found: {label}"));
    };

    window.is_visible().map_err(|err| err.to_string())
}

/// 重新播一次「显示任务栏图标」的当前状态（前端在改过窗口标志之后调用）。
///
/// 置顶 / 穿透这些操作会让 tao 重算窗口样式，把 `WS_EX_TOOLWINDOW` 抹掉，任务栏按钮就回来了
/// （细节见 `windows.rs` 的 `set_taskbar_style`）。这些操作在前端是直接调 Tauri API 的，
/// 所以它们改完之后要顺手叫一次这里。Windows 之外没有这件事，命令保留为空实现。
#[command]
pub async fn refresh_taskbar_visibility<R: Runtime>(app_handle: AppHandle<R>) {
    #[cfg(target_os = "windows")]
    windows::refresh_taskbar_visibility(&app_handle);

    #[cfg(not(target_os = "windows"))]
    let _ = app_handle;
}

fn show_window_by_label(app_handle: &AppHandle, label: &str) {
    let Some(window) = app_handle.get_webview_window(label) else {
        return;
    };

    let app_handle = app_handle.clone();

    spawn(async move {
        show_window(app_handle, window).await;
    });
}

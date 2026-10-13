use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, Runtime, WebviewWindow, command};

use super::{CHAT_WINDOW_LABEL, MAIN_WINDOW_LABEL, REMOTE_CAT_WINDOW_LABEL};

/// 保留跨平台窗口销毁接口；Windows 的置顶状态由系统保持，无后台线程需要停止。
pub fn stop_topmost_keep_alive(_label: &str) {}

/// 贴在桌面上的浮层窗口：自己的猫 / 对方猫 / 聊天窗口。
///
/// 只有这三扇窗跟着「显示任务栏图标」走——偏好设置窗口不在这里：它是个普通窗口，仍照上游
/// 只靠 tao 的任务栏提示。
fn overlay_window_labels() -> [&'static str; 3] {
    [
        MAIN_WINDOW_LABEL,
        REMOTE_CAT_WINDOW_LABEL,
        CHAT_WINDOW_LABEL,
    ]
}

/// 这扇窗是不是贴在桌面上的浮层窗口——只有这三扇跟「显示任务栏图标」走，别的窗口一律不碰。
///
/// 判断放在这里而不是各个调用点：`show_window` / `set_always_on_top` / `show_window_label`
/// 都是通用命令，**谁调用就作用在谁身上**，任何一处漏判都会让偏好设置被顺手套上浮层的样式。
fn is_overlay_window(label: &str) -> bool {
    overlay_window_labels()
        .iter()
        .any(|overlay| *overlay == label)
}

/// 「显示任务栏图标」的当前状态（打开它 = OBS 能在这几扇窗里选到猫）。
///
/// 默认关，与 `tauri.conf.json` 里的 `skipTaskbar: true` 一致；前端那一页载入后会照设置里的
/// 值调一次 `set_taskbar_visibility`。
static TASKBAR_VISIBLE: AtomicBool = AtomicBool::new(false);

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

    restore_taskbar_visibility(&window);

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

    // 改置顶会让 tao 重算整份窗口样式（`WindowState::set_window_flags` 在标志有变化时
    // `SetWindowLongW(GWL_EXSTYLE)`），重算出来的样式里没有我们的 `WS_EX_TOOLWINDOW`。
    // 猫咪右键菜单每次都会走这里（先取消置顶、再恢复），所以不补这一下，任务栏按钮就会跟着
    // 右键冒出来。
    restore_taskbar_visibility(&window);
}

/// 开关「显示任务栏图标」。
///
/// **对整组浮层窗口生效**，不是只对调用它的那一扇。以前只有主窗口那一页绑了这个开关
/// （`src/pages/main/index.vue` 的 `watch`），对方猫与聊天窗口从头到尾没人设置过——用户打开
/// 这个开关想去 OBS 里捕获，列表里只有自己那只猫。
#[command]
pub async fn set_taskbar_visibility<R: Runtime>(app_handle: AppHandle<R>, visible: bool) {
    TASKBAR_VISIBLE.store(visible, Ordering::Relaxed);

    for label in overlay_window_labels() {
        if let Some(window) = app_handle.get_webview_window(label) {
            apply_taskbar_visibility(&window, visible);
        }
    }
}

/// 把开关落到一扇窗上：tao 的记账 + 我们自己的窗口样式。
fn apply_taskbar_visibility<R: Runtime>(window: &WebviewWindow<R>, visible: bool) {
    // 偏好设置不在这里，而且必须在这里挡住：`show_window` 是通用命令，用托盘打开偏好设置
    // 时它拿到的就是偏好设置那扇窗。`WS_EX_TOOLWINDOW` 一旦加上，系统会改用**工具窗口的
    // 紧凑标题栏**——最小化 / 最大化不再画，只剩一个更小的关闭按钮，用户看到的就是
    // 「标题栏三个按钮变成这样了」。偏好设置是普通窗口，仍照上游只靠 tao 的任务栏提示。
    if !is_overlay_window(window.label()) {
        return;
    }

    // tao 会在 `TaskbarCreated`（资源管理器重启）之后照它存的那份值重放一次，
    // 所以这一句不能省——省了它，下次重启资源管理器就会把「显示任务栏图标」关掉。
    let _ = window.set_skip_taskbar(!visible);

    set_taskbar_style(window, visible);
}

/// 按开关改窗口样式：关掉开关 = 上 `WS_EX_TOOLWINDOW`，打开 = 摘掉它并带上 `WS_EX_APPWINDOW`。
///
/// 为什么不能只靠 `set_skip_taskbar`：tao 0.34 的实现只调 `ITaskbarList::DeleteTab`，那是个
/// **提示**——资源管理器重启、或者窗口隐藏后再显示，任务栏按钮都会自己回来（tao 给自家内部
/// 隐藏窗口加 `WS_EX_TOOLWINDOW` 就是绕开这件事，见 tao 的 `create_event_target_window`），
/// 而**无父窗口**创建的窗口必然带 `WS_EX_APPWINDOW`（`WindowFlags::ON_TASKBAR`），所以默认
/// 状态就是「会被画任务栏按钮、只靠提示压着」——用户报的「没开 OBS 捕获有时也出现在任务栏」
/// 就是这个提示丢了。样式是硬的，改完就不会再冒出来（顺带也不进 Alt+Tab）。
fn set_taskbar_style<R: Runtime>(window: &WebviewWindow<R>, visible: bool) {
    let Ok(hwnd) = window.hwnd() else {
        return;
    };

    // 裸指针不是 `Send`，换成整数带进主线程。
    let hwnd = hwnd.0 as isize;

    // **不能就地写**：tauri 的窗口调用（`set_always_on_top` / `set_ignore_cursor_events` /
    // `set_visible`）只是往事件循环投一条消息，`send_user_message` 从不等待；那条「改标志」的
    // 消息很可能在我们这次写**之后**才被处理，而它一处理就会按标志重算整份样式，把刚写上的
    // `WS_EX_TOOLWINDOW` 抹掉。丢进主线程队列就排在那条消息后面了。
    let _ = window.app_handle().run_on_main_thread(move || {
        apply_taskbar_style(hwnd, visible);
    });
}

/// 真正动样式的那一段（只在主线程队列里跑）。
///
/// `hwnd` 传的是整数：窗口在排队期间被销毁的话，user32 对失效句柄只会返回失败，不会崩。
fn apply_taskbar_style(hwnd: isize, visible: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    // tauri 用的 windows crate 与本 crate 依赖的未必是同一个版本，所以只按裸指针重建，
    // 不引它的类型。
    let hwnd = HWND(hwnd as _);

    let tool_window = WS_EX_TOOLWINDOW.0 as isize;
    let app_window = WS_EX_APPWINDOW.0 as isize;

    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);

        let next = if visible {
            (current | app_window) & !tool_window
        } else {
            (current | tool_window) & !app_window
        };

        if next == current {
            return;
        }

        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);

        // 改完要让系统重算非客户区，否则有的配置下样式要等下一次尺寸变化才生效。
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
}

/// 窗口显示出来时补一次任务栏状态（对方猫 / 聊天窗口这类先隐藏、后显示的窗口，
/// 隐藏再显示会让任务栏按钮回来）。
pub fn restore_taskbar_visibility<R: Runtime>(window: &WebviewWindow<R>) {
    apply_taskbar_visibility(window, TASKBAR_VISIBLE.load(Ordering::Relaxed));
}

/// 把当前的「显示任务栏图标」状态重播到所有浮层窗口（前端在改过窗口标志之后调用）。
///
/// 会改窗口标志的操作（置顶 / 穿透 / 显示隐藏）都会让 tao 重算样式并把 `WS_EX_TOOLWINDOW`
/// 抹掉；这些操作大多不在本插件里（前端直接调 `setIgnoreCursorEvents`），所以给它们留一个
/// 「补一次」的入口。
pub fn refresh_taskbar_visibility<R: Runtime>(app_handle: &AppHandle<R>) {
    for label in overlay_window_labels() {
        if let Some(window) = app_handle.get_webview_window(label) {
            restore_taskbar_visibility(&window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三扇浮层窗口跟着开关走，偏好设置不跟。
    ///
    /// 它一旦被顺手套上 `WS_EX_TOOLWINDOW`，系统会改用工具窗口的紧凑标题栏：最小化 /
    /// 最大化不再画，只剩一个更小的关闭按钮——这正是用户报过的现象。
    #[test]
    fn only_the_overlay_windows_follow_the_taskbar_switch() {
        use crate::commands::PREFERENCE_WINDOW_LABEL;

        assert!(is_overlay_window(MAIN_WINDOW_LABEL));
        assert!(is_overlay_window(REMOTE_CAT_WINDOW_LABEL));
        assert!(is_overlay_window(CHAT_WINDOW_LABEL));
        assert!(!is_overlay_window(PREFERENCE_WINDOW_LABEL));
        assert!(!is_overlay_window(""));
    }
}

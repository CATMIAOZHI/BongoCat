use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Runtime, WebviewWindow, command};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
};

/// 每个窗口自己的置顶保持开关，按窗口 label 区分。
///
/// 上游用单个全局开关，只支持一个强制置顶窗口：任何一个窗口关闭置顶都会
/// 停掉别的窗口的保持线程。多窗口（main / remote-cat / chat）必须各自独立。
type TopmostHandle = Arc<AtomicBool>;

static TOPMOST_HANDLES: OnceLock<Mutex<HashMap<String, TopmostHandle>>> = OnceLock::new();

fn topmost_handles() -> MutexGuard<'static, HashMap<String, TopmostHandle>> {
    TOPMOST_HANDLES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn raw_hwnd_of<R: Runtime>(window: &WebviewWindow<R>) -> Option<isize> {
    window.hwnd().ok().map(|hwnd| hwnd.0 as isize)
}

fn clear_topmost(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn spawn_topmost_keep_alive(raw_hwnd: isize, running: TopmostHandle) {
    thread::spawn(move || {
        let hwnd = HWND(raw_hwnd as *mut _);

        while running.load(Ordering::SeqCst) {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }

            // 关闭置顶与保持线程之间存在竞态：线程可能正好在标志被清除之后
            // 又断言了一次 TOPMOST。这里在断言后复查，若已关闭则撤销，保证
            // 任意交错顺序下最终的窗口状态都是「不置顶」。
            if !running.load(Ordering::SeqCst) {
                clear_topmost(raw_hwnd);

                break;
            }

            thread::sleep(Duration::from_millis(16));
        }
    });
}

/// 停止某个窗口的置顶保持线程（窗口销毁时必须调用，否则会一直对失效 HWND 轮询）
pub fn stop_topmost_keep_alive(label: &str) {
    let previous = topmost_handles().remove(label);

    if let Some(previous) = previous {
        previous.store(false, Ordering::SeqCst);
    }
}

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
    let Some(raw_hwnd) = raw_hwnd_of(&window) else {
        return;
    };
    let label = window.label().to_string();

    if always_on_top {
        let running: TopmostHandle = Arc::new(AtomicBool::new(true));

        // 同一个窗口重复开启置顶时，先停掉上一个保持线程，避免线程堆积
        let previous = topmost_handles().insert(label, Arc::clone(&running));

        if let Some(previous) = previous {
            previous.store(false, Ordering::SeqCst);
        }

        spawn_topmost_keep_alive(raw_hwnd, running);
    } else {
        stop_topmost_keep_alive(&label);

        clear_topmost(raw_hwnd);
    }
}

#[command]
pub async fn set_taskbar_visibility<R: Runtime>(window: WebviewWindow<R>, visible: bool) {
    let _ = window.set_skip_taskbar(!visible);
}

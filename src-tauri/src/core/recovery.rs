//! Recovery must not depend on a functioning JavaScript renderer.
use tauri::{AppHandle, menu::MenuEvent};

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::{Duration, SystemTime};
#[cfg(windows)]
use tauri_plugin_custom_window::MAIN_WINDOW_LABEL;
#[cfg(windows)]
use tauri_plugin_log::log;

/// WebViews are created while the app builds its windows, so a healthy start
/// brings the cat's page up within seconds. If it still has none by then,
/// WebView2 itself failed — see `watch_startup`.
#[cfg(windows)]
const WEBVIEW_READY_SECS: u64 = 20;

/// A self-restart is worth exactly one retry: a WebView2 that was merely not
/// ready yet comes up on the second run, while a permanently broken one ends up
/// talking to the user instead of restarting forever.
#[cfg(windows)]
const AUTO_RESTART_WINDOW_SECS: u64 = 600;

#[cfg(windows)]
const RENDERER_FAILED_MESSAGE: &str = "猫猫的界面失去响应或已停止运行，可能会显示白屏。\n\n点击“一键重启”重新打开 BongoCat。未发送的文字或录音可能丢失，联机会重新连接。\n\n暂不重启后，若托盘图标可见，可从托盘菜单重启；若已隐藏托盘，请从任务管理器结束 BongoCat 后重新打开。";

#[cfg(windows)]
const WEBVIEW_START_FAILED_MESSAGE: &str = "猫猫的界面没能启动：系统的 WebView2 组件这时还没准备好，通常在刚登录的时候出现。\n\n点击“一键重启”再试一次。如果一直这样，请重启电脑，或在 Windows 设置里修复 Microsoft Edge WebView2 Runtime。";

/// Set only from `with_webview`, i.e. only for a window whose native WebView
/// really exists. The plugin hook `on_webview_ready` is *not* usable for this:
/// when WebView2 fails to create, tauri-runtime-wry logs the error and drops
/// the window without ever inserting it, yet the hook is still dispatched — so
/// a failed start would look exactly like a healthy one.
#[cfg(windows)]
static MAIN_WEBVIEW_READY: AtomicBool = AtomicBool::new(false);

pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "bongo-restart-native" => app.request_restart(),
        "bongo-exit-native" => app.exit(0),
        _ => {}
    }
}

/// Startup can fail before any page exists, which leaves a running process with
/// no window, no way to click anything and no hint about what happened. Nothing
/// inside the app can report that state (there is no page to run it), so this
/// watches from the native side.
#[cfg(windows)]
pub fn watch_startup(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(WEBVIEW_READY_SECS));

        if MAIN_WEBVIEW_READY.load(Ordering::Relaxed) {
            return;
        }

        log::error!(
            "The main window still has no WebView after {WEBVIEW_READY_SECS}s: WebView2 failed to start"
        );

        if claim_auto_restart(&app) {
            log::warn!("Restarting after a failed WebView2 start");
            app.request_restart();
            return;
        }

        offer_restart(&app, WEBVIEW_START_FAILED_MESSAGE);
    });
}

/// True only when the marker has been written, so an unwritable data directory
/// can never turn into an endless restart loop.
#[cfg(windows)]
fn claim_auto_restart(app: &AppHandle) -> bool {
    use tauri::Manager;

    let Ok(dir) = app.path().app_local_data_dir() else {
        return false;
    };

    let stamp = dir.join("webview-recovery.stamp");
    let last_restart = std::fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .ok();

    if !auto_restart_allowed(last_restart, SystemTime::now()) {
        return false;
    }

    std::fs::create_dir_all(&dir).is_ok()
        && std::fs::write(&stamp, "automatic restart after a failed WebView2 start\n").is_ok()
}

#[cfg(windows)]
fn auto_restart_allowed(last_restart: Option<SystemTime>, now: SystemTime) -> bool {
    match last_restart {
        None => true,
        // A stamp from the future (clock moved back) must not open the loop either.
        Some(last) => now.duration_since(last).map_or(false, |age| {
            age >= Duration::from_secs(AUTO_RESTART_WINDOW_SECS)
        }),
    }
}

#[cfg(windows)]
fn offer_restart(app: &AppHandle, message: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let restart_app = app.clone();
    app.dialog()
        .message(message)
        .title("BongoCat · 界面异常")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "一键重启".into(),
            "暂不重启".into(),
        ))
        .show(move |restart| {
            if restart {
                restart_app.request_restart();
            }
        });
}

#[cfg(windows)]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use std::sync::Arc;
    use tauri::Manager;
    use webview2_com::{Microsoft::Web::WebView2::Win32::*, ProcessFailedEventHandler};

    // A browser failure can notify every window. Offer recovery only once per
    // app run, including after dismissal, rather than opening a dialog storm.
    let offered = Arc::new(AtomicBool::new(false));
    tauri::plugin::Builder::new("renderer-recovery")
        .on_webview_ready(move |webview| {
            let app = webview.app_handle().clone();
            let label = webview.label().to_owned();
            let is_main = label == MAIN_WINDOW_LABEL;
            let offered = offered.clone();
            if let Err(error) = webview.with_webview(move |platform| {
                let registered = unsafe {
                    platform.controller().CoreWebView2().and_then(|core| {
                        // Reaching here is proof that the native WebView2 exists,
                        // which is what `watch_startup` waits for.
                        if is_main {
                            MAIN_WEBVIEW_READY.store(true, Ordering::Relaxed);
                        }
                        let handler =
                            ProcessFailedEventHandler::create(Box::new(move |_, args| {
                                let Some(args) = args else { return Ok(()) };
                                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                                args.ProcessFailedKind(&mut kind)?;
                                // GPU/utility exits can be recovered by WebView2 itself.
                                if !needs_recovery(kind) {
                                    return Ok(());
                                }
                                if offered.swap(true, Ordering::Relaxed) {
                                    return Ok(());
                                }
                                log::error!(
                                    "WebView recovery offered: window={label}, kind={}",
                                    kind.0
                                );
                                offer_restart(&app, RENDERER_FAILED_MESSAGE);
                                Ok(())
                            }));
                        // WebView2 retains the handler until this WebView is destroyed.
                        let mut token = 0;
                        core.add_ProcessFailed(&handler, &mut token)
                    })
                };
                if let Err(error) = registered {
                    log::warn!("Failed to register WebView recovery: {error}");
                }
            }) {
                log::warn!("Failed to access WebView for recovery: {error}");
            }
        })
        .build()
}

#[cfg(windows)]
fn needs_recovery(
    kind: webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PROCESS_FAILED_KIND,
) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    matches!(
        kind,
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
            | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
            | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use webview2_com::Microsoft::Web::WebView2::Win32::*;

    #[test]
    fn only_failures_affecting_the_page_offer_restart() {
        assert!(needs_recovery(
            COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
        ));
        assert!(needs_recovery(
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
        ));
        assert!(needs_recovery(
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
        ));
        assert!(!needs_recovery(
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED
        ));
        assert!(!needs_recovery(
            COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED
        ));
    }

    #[test]
    fn a_recent_self_restart_blocks_the_next_one() {
        let now = SystemTime::now();

        assert!(auto_restart_allowed(None, now));
        assert!(!auto_restart_allowed(
            Some(now - Duration::from_secs(30)),
            now
        ));
        assert!(auto_restart_allowed(
            Some(now - Duration::from_secs(AUTO_RESTART_WINDOW_SECS)),
            now
        ));
        // A stamp from the future (clock moved back) must not trap the user in dialogs.
        assert!(!auto_restart_allowed(
            Some(now + Duration::from_secs(60)),
            now
        ));
    }
}

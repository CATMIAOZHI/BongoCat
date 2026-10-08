//! Recovery must not depend on a functioning JavaScript renderer.
use tauri::{AppHandle, menu::MenuEvent};

pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "bongo-restart-native" => app.request_restart(),
        "bongo-exit-native" => app.exit(0),
        _ => {}
    }
}

#[cfg(windows)]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tauri::Manager;
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    use tauri_plugin_log::log;
    use webview2_com::{Microsoft::Web::WebView2::Win32::*, ProcessFailedEventHandler};

    // A browser failure can notify every window. Offer recovery only once per
    // app run, including after dismissal, rather than opening a dialog storm.
    let offered = Arc::new(AtomicBool::new(false));
    tauri::plugin::Builder::new("renderer-recovery")
        .on_webview_ready(move |webview| {
            let app = webview.app_handle().clone();
            let label = webview.label().to_owned();
            let offered = offered.clone();
            if let Err(error) = webview.with_webview(move |platform| {
                let registered = unsafe {
                    platform.controller().CoreWebView2().and_then(|core| {
                        let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
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
                            log::error!("WebView recovery offered: window={label}, kind={}", kind.0);
                            let restart_app = app.clone();
                            app.dialog()
                                .message("猫猫的界面失去响应或已停止运行，可能会显示白屏。\n\n点击“一键重启”重新打开 BongoCat。未发送的文字或录音可能丢失，联机会重新连接。\n\n暂不重启后，若托盘图标可见，可从托盘菜单重启；若已隐藏托盘，请从任务管理器结束 BongoCat 后重新打开。")
                                .title("BongoCat · 界面异常")
                                .kind(MessageDialogKind::Warning)
                                .buttons(MessageDialogButtons::OkCancelCustom(
                                    "一键重启".into(), "暂不重启".into(),
                                ))
                                .show(move |restart| {
                                    if restart {
                                        restart_app.request_restart();
                                    }
                                });
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
}

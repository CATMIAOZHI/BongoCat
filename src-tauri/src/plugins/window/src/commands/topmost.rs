//! Windows 置顶自愈：低频读取真实窗口层级，只有偏离设置时才调整，不抢输入焦点。
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tauri::{AppHandle, Manager, RunEvent, Runtime, WindowEvent};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        UI::WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
            GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindowVisible, SWP_NOACTIVATE,
            SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SetWindowPos, WS_EX_TOPMOST,
        },
    },
    core::BOOL,
};

#[derive(Default)]
struct Guard {
    // 只记启用的窗口；HWND 用于排除同 label 销毁重建后的旧记录。
    enabled: Mutex<HashMap<String, isize>>,
    pending: AtomicBool,
    stopped: AtomicBool,
}

pub(crate) fn start<R: Runtime>(app: &AppHandle<R>) -> std::io::Result<()> {
    let guard = Arc::new(Guard::default());
    app.manage(guard.clone());
    let app = app.clone();
    std::thread::Builder::new()
        .name("window-topmost-check".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if guard.stopped.load(Ordering::Relaxed) {
                    break;
                }
                if guard.enabled.lock().unwrap().is_empty()
                    || guard.pending.swap(true, Ordering::Relaxed)
                {
                    continue;
                }
                let queued_app = app.clone();
                let queued_guard = guard.clone();
                // 忙碌时最多留一条任务，不能每两秒往卡住的主线程堆积闭包。
                if app
                    .run_on_main_thread(move || {
                        if !queued_guard.stopped.load(Ordering::Relaxed) {
                            check(&queued_app, &queued_guard);
                        }
                        queued_guard.pending.store(false, Ordering::Relaxed);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(())
}

pub(crate) fn on_event<R: Runtime>(app: &AppHandle<R>, event: &RunEvent) {
    let guard = app.state::<Arc<Guard>>();
    match event {
        RunEvent::Exit => guard.stopped.store(true, Ordering::Relaxed),
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::Destroyed,
            ..
        } => {
            guard.enabled.lock().unwrap().remove(label);
        }
        _ => {}
    }
}

/// 与 Tauri 设置排在同一个主线程闭包里；右键菜单临时取消置顶也会移除记录。
pub(super) fn remember<R: Runtime>(app: &AppHandle<R>, label: &str, hwnd: isize, enabled: bool) {
    let guard = app.state::<Arc<Guard>>();
    let mut windows = guard.enabled.lock().unwrap();
    if enabled {
        windows.insert(label.to_owned(), hwnd);
    } else {
        windows.remove(label);
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    hwnd: isize,
    process: u32,
    bounds: RECT,
    topmost: bool,
}

impl Snapshot {
    fn read(hwnd: HWND) -> Option<Self> {
        unsafe {
            // 隐藏、最小化的窗口不能被守护逻辑拉出来。
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return None;
            }
            let mut bounds = RECT::default();
            GetWindowRect(hwnd, &mut bounds).ok()?;
            let mut process = 0;
            if GetWindowThreadProcessId(hwnd, Some(&mut process)) == 0 {
                return None;
            }
            Some(Self {
                hwnd: hwnd.0 as isize,
                process,
                bounds,
                topmost: GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0,
            })
        }
    }
}

/// EnumWindows 提供本次 Z 序快照，避免用 GetWindow 循环追逐不断变化的窗口链。
unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let windows = unsafe { &mut *(data.0 as *mut Vec<isize>) };
    windows.push(hwnd.0 as isize);
    BOOL(1)
}

fn overlaps(a: RECT, b: RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn needs_repair(cat: Snapshot, foreground: Option<Snapshot>, z_order: &[isize]) -> bool {
    if !cat.topmost {
        return true;
    }
    let Some(front) = foreground else {
        return false;
    };
    // 猫、聊天、设置、右键菜单互相不争抢。只恢复被外部前台窗口实际盖住的情况，
    // 不追着系统提示、工具提示等后台浮层强行抬高。
    if front.process == cat.process || !overlaps(cat.bounds, front.bounds) {
        return false;
    }
    match (
        z_order.iter().position(|hwnd| *hwnd == front.hwnd),
        z_order.iter().position(|hwnd| *hwnd == cat.hwnd),
    ) {
        (Some(front), Some(cat)) => front < cat,
        _ => false,
    }
}

fn check<R: Runtime>(app: &AppHandle<R>, guard: &Guard) {
    // 执行时再读取意图，不使用定时线程的旧快照；调用原生 API 时不持锁。
    let enabled = guard.enabled.lock().unwrap().clone();
    let mut cats = Vec::new();
    for (label, expected_hwnd) in enabled {
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        let Ok(hwnd) = window.hwnd() else {
            continue;
        };
        if hwnd.0 as isize != expected_hwnd {
            continue;
        }
        if let Some(cat) = Snapshot::read(HWND(expected_hwnd as _)) {
            cats.push(cat);
        }
    }
    if cats.is_empty() {
        return;
    }
    let foreground = Snapshot::read(unsafe { GetForegroundWindow() });
    let mut z_order = Vec::<isize>::new();
    if unsafe {
        EnumWindows(
            Some(collect_window),
            LPARAM(&mut z_order as *mut Vec<isize> as isize),
        )
    }
    .is_err()
    {
        return;
    }
    for hwnd in repair_order(&cats, foreground, &z_order) {
        raise(hwnd);
    }
}

fn repair_order(cats: &[Snapshot], foreground: Option<Snapshot>, z_order: &[isize]) -> Vec<isize> {
    if !cats
        .iter()
        .any(|cat| needs_repair(*cat, foreground, z_order))
    {
        return Vec::new();
    }
    // 任一成员失效才恢复整组（最多三扇）。只抬失效的猫会越过原本在它上方的健康聊天窗；
    // 由低到高抬整组才能保留相对顺序。健康轮次完全不调用 SetWindowPos。
    z_order
        .iter()
        .rev()
        .filter(|hwnd| cats.iter().any(|cat| cat.hwnd == **hwnd))
        .copied()
        .collect()
}

pub(super) fn raise(hwnd: isize) {
    unsafe {
        // 原生调用绕过 tao 的 ALWAYS_ON_TOP 缓存：缓存为 true 时再次 set(true) 不会
        // 调 SetWindowPos，无法修复外部程序改动后的层级。NOACTIVATE 保持游戏/打字焦点。
        let _ = SetWindowPos(
            HWND(hwnd as _),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(hwnd: isize, process: u32, topmost: bool) -> Snapshot {
        Snapshot {
            hwnd,
            process,
            topmost,
            bounds: RECT {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
        }
    }

    #[test]
    fn repairs_lost_flag_and_foreground_occlusion_but_not_healthy_z_order() {
        let cat = window(1, 10, true);
        let foreground = window(2, 20, true);
        assert!(needs_repair(window(1, 10, false), None, &[1]));
        assert!(needs_repair(cat, Some(foreground), &[2, 1]));
        assert!(!needs_repair(cat, Some(foreground), &[1, 2]));
        assert!(!needs_repair(cat, None, &[1]));
        assert!(!needs_repair(cat, Some(foreground), &[1]));
    }

    #[test]
    fn does_not_fight_our_windows_or_non_overlapping_foreground() {
        let cat = window(1, 10, true);
        assert!(!needs_repair(cat, Some(window(2, 10, true)), &[2, 1]));
        let mut other = window(2, 20, true);
        other.bounds.left = 100;
        other.bounds.right = 200;
        assert!(!needs_repair(cat, Some(other), &[2, 1]));
        other.bounds.left = -200;
        other.bounds.right = -100;
        assert!(!needs_repair(cat, Some(other), &[2, 1]));
    }

    #[test]
    fn repairing_a_lower_cat_preserves_the_healthy_chat_above_it() {
        let cat = window(1, 10, true);
        let chat = window(3, 10, true);
        let external = window(2, 20, true);
        assert_eq!(
            repair_order(&[cat, chat], Some(external), &[3, 2, 1]),
            vec![1, 3]
        );
        assert!(repair_order(&[cat, chat], Some(external), &[3, 1, 2]).is_empty());
        // 取消置顶/隐藏的窗口不在 cats 集合，不能被顺带抬起。
        assert_eq!(repair_order(&[cat], Some(external), &[3, 2, 1]), vec![1]);
        assert!(repair_order(&[], Some(external), &[3, 2, 1]).is_empty());
    }
}

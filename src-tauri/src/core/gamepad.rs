use gilrs::{EventType, Gilrs};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Runtime, command};

static IS_LISTENING: AtomicBool = AtomicBool::new(false);

/// 没事件时最多等多久（见下面的读法）。
///
/// 它**不是**事件延迟：`recv_timeout` 在通道里一有消息就立刻返回，这个超时只决定「空转时多久
/// 醒一次」（16ms = 62.5 次/秒）与 `stop_gamepad_listing` 的生效时延。真正的延迟下界在 gilrs
/// 自己的生产者线程里——本工程用的是 `features = ["xinput"]`，那条线程每 10ms 才轮询一次手柄
/// 状态（`gilrs-core` 的 `platform/windows_xinput/gamepad.rs` 里 `EVENT_THREAD_SLEEP_TIME = 10`）。
/// 所以这个数只要明显小于「可感知」就够了，选 16ms 是为了和别处的 60Hz 对齐。
const GAMEPAD_POLL_TIMEOUT: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Serialize)]
pub enum GamepadEventKind {
    ButtonChanged,
    AxisChanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct GamepadEvent {
    kind: GamepadEventKind,
    name: String,
    value: f32,
}

#[command]
pub async fn start_gamepad_listing<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), String> {
    if IS_LISTENING.load(Ordering::SeqCst) {
        return Ok(());
    }

    IS_LISTENING.store(true, Ordering::SeqCst);

    let mut gilrs = Gilrs::new().map_err(|err| err.to_string())?;

    while IS_LISTENING.load(Ordering::SeqCst) {
        // `next_event()` 在 Windows 后端上就是 `try_recv()`：没事件时**立刻**返回 `None`，
        // 所以原来那个 `while let` 外面套一圈 = 纯自旋，手柄模型一旦被选上就有一个核一直
        // 100% 空转（16 核机器上任务管理器只显示约 6%，但整机因为少了一个核而发卡）。
        // 阻塞版在同一个通道上 `recv_timeout`：没事件时让出 CPU，有事件时立刻醒。
        let Some(event) = gilrs.next_event_blocking(Some(GAMEPAD_POLL_TIMEOUT)) else {
            continue;
        };

        let gamepad_event = match event.event {
            EventType::ButtonChanged(button, value, ..) => GamepadEvent {
                kind: GamepadEventKind::ButtonChanged,
                name: format!("{:?}", button),
                value,
            },
            EventType::AxisChanged(axis, value, ..) => GamepadEvent {
                kind: GamepadEventKind::AxisChanged,
                name: format!("{:?}", axis),
                value,
            },
            _ => continue,
        };

        let _ = app_handle.emit("gamepad-changed", gamepad_event);
    }

    Ok(())
}

#[command]
pub async fn stop_gamepad_listing() {
    if !IS_LISTENING.load(Ordering::SeqCst) {
        return;
    }

    IS_LISTENING.store(false, Ordering::SeqCst);
}

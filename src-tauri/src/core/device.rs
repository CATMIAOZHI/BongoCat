use rdev::{Event, EventType, listen};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use tauri::{AppHandle, Emitter, Runtime, command};

#[derive(Debug, Clone, Serialize)]
pub enum DeviceEventKind {
    MousePress,
    MouseRelease,
    MouseMove,
    KeyboardPress,
    KeyboardRelease,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceEvent {
    kind: DeviceEventKind,
    value: Value,
}

static IS_LISTENING: AtomicBool = AtomicBool::new(false);

/// rdev 的原始键名 → 平台键码（Windows 上就是虚拟键码 `vkCode`）。
///
/// 给 [`is_key_down`] 用：前端的「按键自动释放」不能把「安静」当成「抬起」——Windows 的
/// 键盘自动重复只跟**最后按下**的那个键，被后来者压住的键会完全安静下来而且不会恢复，
/// 所以到点时必须问一次系统。键码只能从事件里学到，这里按下与抬起时各记一份。
///
/// 一种记不准的情形：rdev 的 `get_code()` 在 `vkCode == VK_PACKET` 时返回的是**扫描码**，
/// 而被 `SendInput(KEYEVENTF_UNICODE)` 注入的按键（语音输入、宏、部分输入法）才会走那条。
/// 那时键名是从扫描码翻出来的（名字对），码却不是一个能查得到的虚拟键码——问系统等于在问
/// 另一个键。影响很小：注入照例连抬起一起发，而前端在收到抬起时就撤掉了那一轮、不会再问；
/// 真把抬起丢了也只是可能答成「还按着」，等那个不相干的键松开就自愈。这里不特殊处理——
/// 分辨不出来，而按扫描码把键名丢掉反而会让这个键退回到点释放。
static KEY_CODES: LazyLock<Mutex<HashMap<String, i32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[command]
pub async fn start_device_listening<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), String> {
    if IS_LISTENING.load(Ordering::SeqCst) {
        return Ok(());
    }

    IS_LISTENING.store(true, Ordering::SeqCst);

    let callback = move |event: Event| {
        // 键码只在键盘事件上有意义（鼠标事件上是 0）。`platform_code` 在 Windows 上就是
        // `vkCode`，键名与下面 emit 给前端的是同一个（`{:?}` 出来的枚举名）
        if let EventType::KeyPress(key) | EventType::KeyRelease(key) = &event.event_type {
            if event.platform_code != 0 {
                KEY_CODES
                    .lock()
                    .unwrap()
                    .insert(format!("{key:?}"), event.platform_code as i32);
            }
        }

        let device_event = match event.event_type {
            EventType::ButtonPress(button) => DeviceEvent {
                kind: DeviceEventKind::MousePress,
                value: json!(format!("{:?}", button)),
            },
            EventType::ButtonRelease(button) => DeviceEvent {
                kind: DeviceEventKind::MouseRelease,
                value: json!(format!("{:?}", button)),
            },
            EventType::MouseMove { x, y } => DeviceEvent {
                kind: DeviceEventKind::MouseMove,
                value: json!({ "x": x, "y": y }),
            },
            EventType::KeyPress(key) => DeviceEvent {
                kind: DeviceEventKind::KeyboardPress,
                value: json!(format!("{:?}", key)),
            },
            EventType::KeyRelease(key) => DeviceEvent {
                kind: DeviceEventKind::KeyboardRelease,
                value: json!(format!("{:?}", key)),
            },
            _ => return,
        };

        let _ = app_handle.emit("device-changed", device_event);
    };

    listen(callback).map_err(|err| format!("Failed to listen device: {:?}", err))?;

    Ok(())
}

/// `keys` 里有没有还按着的键（Windows 上查 `GetAsyncKeyState`）。
///
/// 调用方（`useDevice` 的按键自动释放）只在这一种判断上需要它：一个安静了很久的键，
/// 到底是真抬起了，还是被后来按下的键压住了自动重复。
///
/// 键名是 rdev 的原始名（`KeyW` / `ShiftLeft`）；没见过的键名一律算「不在按下」，
/// 调用方据此保留原来的「到点就释放」行为。非 Windows 直接返回 false（那边不做自动释放）。
#[command]
pub fn is_key_down(keys: Vec<String>) -> bool {
    #[cfg(windows)]
    {
        use winapi::um::winuser::GetAsyncKeyState;

        let codes = KEY_CODES.lock().unwrap();

        keys.iter().any(|name| {
            let Some(code) = codes.get(name) else {
                return false;
            };

            // 最高位（0x8000）置位 = 这个键现在按着
            (unsafe { GetAsyncKeyState(*code) } as u32 & 0x8000) != 0
        })
    }

    #[cfg(not(windows))]
    {
        let _ = keys;

        false
    }
}

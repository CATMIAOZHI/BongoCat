use rdev::{Event, EventType, listen};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
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

/// 鼠标移动的「最新那个坐标」（`f64` 的位模式）与「有新值」标志。
///
/// 钩子回调跑在**系统输入的关键路径**上：`rdev` 是在低层钩子过程里**同步**调用我们的回调的
/// （它 `windows/listen.rs` 里就是先 `callback(event)`、之后才 `CallNextHookEx`），所以回调
/// 里干的每一件事都直接加在下一次鼠标移动的延迟上。原来这里是「每一次移动 → `json!` 建一份
/// 值 → `emit`（序列化成字符串、拼一小段 JS 源码、`PostMessageW` 到窗口线程）」，1000Hz 的
/// 鼠标就是每秒上千次。而下游要的只是**最新那一个坐标**：本机那只会跟着 60fps 的插值走，
/// 联机快照本来就被 `petStateHz` 限到 60Hz。
///
/// 所以钩子里只做三次原子写，由一个 60Hz 的循环合并后发一次（见 [`coalesce_mouse_moves`]）。
static MOUSE_X: AtomicU64 = AtomicU64::new(0);
static MOUSE_Y: AtomicU64 = AtomicU64::new(0);
static MOUSE_PENDING: AtomicBool = AtomicBool::new(false);

/// 鼠标位置合并后往上发的频率（Hz）。与前端那份平滑（`useDevice` 的 `Ticker`，60fps）对齐，
/// 高于它只是白跑。
const MOUSE_EMIT_HZ: u64 = 60;

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

    // 合并鼠标移动的那条线程（见 `MOUSE_X` 的注释）：与钩子线程分开，钩子只负责记坐标。
    //
    // 这次 spawn **必须**排在上面 `store(true)` 之后：新线程每次循环都先读 `IS_LISTENING`，
    // 排在前面就有机会读到 `false` 直接退出（那是这条线程唯一的退出路径）。
    let coalescer = app_handle.clone();

    std::thread::spawn(move || coalesce_mouse_moves(coalescer));

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
            EventType::MouseMove { x, y } => {
                // 这里**不能**直接发（见 `MOUSE_X` 的注释）：只记坐标，最后置一次标志就返回。
                //
                // 三个原子写之间**不**保证「合并线程读到的 x 与 y 来自同一次移动」——读者可能在
                // 两次写之间插进来，于是拿到拼接的一对。写者只有钩子这一个线程、两次写相隔几纳秒，
                // 读者 16ms 才读一次：单次写落进读者那两次 load 之间的概率是「几纳秒 ÷ 16ms」，
                // 一拍里有十几次写，所以每拍约是它的十几倍——仍然约万分之一，而偏差最多是一次移动
                // 的位移（平稳移动时 1px，甩鼠标时可以是几 px）。
                //
                // 要彻底消除得做 seqlock（写者先写一个奇/偶版本号），最省的写法是在钩子里加两次
                // 普通 Release 写——x86 上就是两条 `mov`，不需要读改写或额外的 fence。即便如此也
                // 不值得：为一个看不出来的偏差，再往系统输入的关键路径上加东西。
                MOUSE_X.store(x.to_bits(), Ordering::Relaxed);
                MOUSE_Y.store(y.to_bits(), Ordering::Relaxed);
                MOUSE_PENDING.store(true, Ordering::Release);

                return;
            }
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

/// 把钩子记下的坐标按 [`MOUSE_EMIT_HZ`] 合并后发出去（见 [`MOUSE_X`] 的注释）。
///
/// 「有变化才发」而不是「每拍都发」：鼠标不动的时候一帧都不发（每拍仍会醒一次、做一次原子交换，
/// 那是可以忽略的开销）。位置没变时前端本来也不会看到任何区别（插值早就稳定在同一个点上）。
fn coalesce_mouse_moves<R: Runtime>(app_handle: AppHandle<R>) {
    let interval = Duration::from_millis(1000 / MOUSE_EMIT_HZ);

    loop {
        // 这条线程与进程同寿（`rdev::listen` 是消息循环，不返回），留个退出条件给以后可能加的
        // 「停止监听」——现在没人会把它置回 false。
        if !IS_LISTENING.load(Ordering::SeqCst) {
            return;
        }

        std::thread::sleep(interval);

        if !MOUSE_PENDING.swap(false, Ordering::Acquire) {
            continue;
        }

        let event = DeviceEvent {
            kind: DeviceEventKind::MouseMove,
            value: json!({
                "x": f64::from_bits(MOUSE_X.load(Ordering::Relaxed)),
                "y": f64::from_bits(MOUSE_Y.load(Ordering::Relaxed)),
            }),
        };

        let _ = app_handle.emit("device-changed", &event);
    }
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

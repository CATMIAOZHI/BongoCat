use rdev::{Event, EventType, listen};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Runtime, command};
use tauri_plugin_log::log::error;

#[cfg(windows)]
mod pointer;

#[cfg(not(windows))]
use std::sync::atomic::AtomicU64;

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
/// **只有非 Windows 用**：Windows 上钩子连鼠标那一半都不装（见 `init_keyboard_only`），
/// 鼠标位姿改由 `pointer.rs` 轮询提供，这边没有东西要合并。
///
/// 钩子回调跑在**系统输入的关键路径**上：`rdev` 是在低层钩子过程里**同步**调用我们的回调的
/// （它 `windows/listen.rs` 里就是先 `callback(event)`、之后才 `CallNextHookEx`），所以回调
/// 里干的每一件事都直接加在下一次鼠标移动的延迟上。原来这里是「每一次移动 → `json!` 建一份
/// 值 → `emit`（序列化成字符串、拼一小段 JS 源码、`PostMessageW` 到窗口线程）」，1000Hz 的
/// 鼠标就是每秒上千次。而下游要的只是**最新那一个坐标**：本机那只会跟着 60fps 的插值走，
/// 联机快照本来就被 `petStateHz` 限到 60Hz。
///
/// 所以钩子里只做三次原子写，由一个 60Hz 的循环合并后发一次（见 [`coalesce_mouse_moves`]）。
#[cfg(not(windows))]
static MOUSE_X: AtomicU64 = AtomicU64::new(0);
#[cfg(not(windows))]
static MOUSE_Y: AtomicU64 = AtomicU64::new(0);
#[cfg(not(windows))]
static MOUSE_PENDING: AtomicBool = AtomicBool::new(false);

/// 鼠标位姿采样 / 合并的节拍（Hz）。与前端那份平滑（`useDevice` 的 `Ticker`，60fps）对齐，
/// 高于它只是白跑。Windows 上这是鼠标位姿**唯一**的来源（见 `pointer.rs` 的模块注释）；
/// 其它平台它只是「钩子记下的最新坐标」往上发的频率。
///
/// `Duration::from_millis(16)` 在 Windows 上实际会睡到系统定时器粒度的整数倍（约 15.6ms），
/// 也就是这一拍真实是 16~31ms。今天就长这样，别顺手改节拍。
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
static KEY_CODES: LazyLock<Mutex<HashMap<String, i32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 让 rdev 只装键盘钩子，**不装鼠标钩子**。
///
/// 必须在 `rdev::listen` 之前调用，所以放在 `run()` 的第一行：rdev 的 `listen()` 里读一次
/// `KEYBOARD_ONLY`（读的是进程环境变量，非空即开），据此跳过 `WH_MOUSE_LL`。
/// 鼠标位姿因此改由 `pointer.rs` 轮询提供。
///
/// 为什么非改不可：低层鼠标钩子让系统在**每一次鼠标移动**时都要唤醒并调度装钩子的那个线程
/// （1000Hz 的鼠标就是每秒上千次），代价直接加在全系统的输入延迟上。回调本身已经不慢了，
/// 贵的是那上千次线程调度——表现是整台机器的鼠标发涩，而 CPU 并没有占满。
///
/// 只在 Windows 上设：mac / linux 的 rdev 也读同一个变量，但那边今天没有这个问题，
/// 不动它们的钩子集合。
#[cfg(windows)]
pub fn init_keyboard_only() {
    // SAFETY: `set_var` 在 edition 2024 里是 unsafe（进程里可能正有别的线程读环境变量）。
    // 这个调用点在最早期，此刻只有主线程在跑；装钩子的线程是后面才 spawn 的，
    // 它读到的一定是这份值。
    unsafe { std::env::set_var("KEYBOARD_ONLY", "1") };
}

#[command]
pub fn start_device_listening<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), String> {
    if IS_LISTENING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    // 只传不含字符串的离散事件；鼠标移动不进队列。保持 press/release/repeat 顺序，
    // 不用有界队列丢弃 release，也不在钩子里等待消费者。消费者的 emit 只是投递窗口消息。
    let (sender, receiver) = mpsc::channel::<(EventType, u32)>();
    let dispatcher = app_handle.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("device-events".into())
        .spawn(move || {
            for (event_type, platform_code) in receiver {
                dispatch_device_event(&dispatcher, event_type, platform_code);
            }
        })
    {
        IS_LISTENING.store(false, Ordering::SeqCst);
        return Err(format!("Failed to start device dispatcher: {err}"));
    }

    // 鼠标位姿那条线程（见 `MOUSE_EMIT_HZ` 与 `pointer.rs` 的模块注释）：与钩子线程分开，
    // 这样落在系统输入关键路径上的就只有钩子那一条线程。
    //
    // 这次 spawn **必须**排在上面 `store(true)` 之后：新线程每次循环都先读 `IS_LISTENING`，
    // 排在前面就有机会读到 `false` 直接退出（那是这条线程唯一的退出路径）。
    let tracker = app_handle.clone();

    #[cfg(windows)]
    {
        if let Err(err) = std::thread::Builder::new()
            .name("device-pointer".into())
            .spawn(move || sample_pointer(tracker))
        {
            IS_LISTENING.store(false, Ordering::SeqCst);
            return Err(format!("Failed to start pointer sampler: {err}"));
        }
    }

    #[cfg(not(windows))]
    std::thread::spawn(move || coalesce_mouse_moves(tracker));

    let callback = move |event: Event| {
        // Windows 上钩子连鼠标那一半都没装（见 `init_keyboard_only`），下面那个分支到不了。
        // 就算将来有人把 `KEYBOARD_ONLY` 拿掉，这里也要显式丢掉：鼠标位姿只有
        // `pointer.rs` 一个来源，两边都发就会打架（同一次按下报两遍、位置两条腿各推一份）。
        #[cfg(windows)]
        {
            if matches!(
                event.event_type,
                EventType::ButtonPress(_)
                    | EventType::ButtonRelease(_)
                    | EventType::MouseMove { .. }
            ) {
                return;
            }
        }

        match event.event_type {
            // 其它平台照旧：这次移动只记坐标，最后一拍合并发一次（见 `MOUSE_X`）。
            #[cfg(not(windows))]
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
            EventType::KeyPress(_)
            | EventType::KeyRelease(_)
            | EventType::ButtonPress(_)
            | EventType::ButtonRelease(_) => {
                let _ = sender.send((event.event_type, event.platform_code));
            }
            _ => return,
        }
    };

    // 装钩子的那条线程。低层钩子的回调就跑在**装钩子的那个线程**上，所以这条线程是系统输入的
    // 关键路径——回调里只许做原子写和一次 `send`。
    //
    // 用自己命名的系统线程，而不是 `tauri::async_runtime::spawn_blocking`：`listen` 是
    // `GetMessageA` 消息循环，一辈子不返回，永久占着 Tokio 阻塞池里的一个槽位没有意义，
    // 而且线程名在调试器 / Process Explorer 里看不出来。
    let hooks = std::thread::Builder::new().name("device-hooks".into());

    if let Err(err) = hooks.spawn(move || {
        if let Err(err) = listen(callback) {
            error!("Failed to listen device: {err:?}");
        }
    }) {
        IS_LISTENING.store(false, Ordering::SeqCst);
        return Err(format!("Failed to start device listener: {err}"));
    }

    Ok(())
}

/// 格式化、键码登记和 WebView 消息投递都离开系统输入钩子。
fn dispatch_device_event<R: Runtime>(
    app: &AppHandle<R>,
    event_type: EventType,
    platform_code: u32,
) {
    if let EventType::KeyPress(key) | EventType::KeyRelease(key) = event_type {
        if platform_code != 0 {
            KEY_CODES
                .lock()
                .unwrap()
                .insert(format!("{key:?}"), platform_code as i32);
        }
    }
    if let Some(event) = discrete_device_event(event_type) {
        let _ = app.emit("device-changed", event);
    }
}

fn discrete_device_event(event_type: EventType) -> Option<DeviceEvent> {
    let (kind, value) = match event_type {
        EventType::ButtonPress(button) => (DeviceEventKind::MousePress, format!("{button:?}")),
        EventType::ButtonRelease(button) => (DeviceEventKind::MouseRelease, format!("{button:?}")),
        EventType::KeyPress(key) => (DeviceEventKind::KeyboardPress, format!("{key:?}")),
        EventType::KeyRelease(key) => (DeviceEventKind::KeyboardRelease, format!("{key:?}")),
        _ => return None,
    };
    Some(DeviceEvent {
        kind,
        value: json!(value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdev::{Button, Key};

    #[test]
    fn discrete_dispatch_preserves_chords_repeats_and_releases() {
        // W 一直按着，A/D 按下再释放：队列不能把 W 的重复或任意 release 合并掉。
        let input = [
            EventType::KeyPress(Key::KeyW),
            EventType::KeyPress(Key::KeyW),
            EventType::KeyPress(Key::KeyA),
            EventType::KeyPress(Key::KeyD),
            EventType::KeyRelease(Key::KeyA),
            EventType::KeyRelease(Key::KeyD),
            EventType::ButtonPress(Button::Left),
            EventType::ButtonRelease(Button::Left),
            EventType::KeyRelease(Key::KeyW),
        ];
        let (tx, rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for event in input {
                tx.send(event).unwrap();
            }
        });
        let actual: Vec<_> = rx
            .into_iter()
            .map(|event| serde_json::to_value(discrete_device_event(event).unwrap()).unwrap())
            .collect();
        producer.join().unwrap();
        assert_eq!(
            actual,
            json!([
                {"kind":"KeyboardPress","value":"KeyW"},
                {"kind":"KeyboardPress","value":"KeyW"},
                {"kind":"KeyboardPress","value":"KeyA"},
                {"kind":"KeyboardPress","value":"KeyD"},
                {"kind":"KeyboardRelease","value":"KeyA"},
                {"kind":"KeyboardRelease","value":"KeyD"},
                {"kind":"MousePress","value":"Left"},
                {"kind":"MouseRelease","value":"Left"},
                {"kind":"KeyboardRelease","value":"KeyW"}
            ])
            .as_array()
            .unwrap()
            .to_vec()
        );
        assert!(discrete_device_event(EventType::MouseMove { x: 1.0, y: 2.0 }).is_none());
    }
}

/// 把钩子记下的坐标按 [`MOUSE_EMIT_HZ`] 合并后发出去（见 [`MOUSE_X`] 的注释）。
/// **非 Windows 专用**：Windows 上这条腿换成了 `sample_pointer`。
///
/// 「有变化才发」而不是「每拍都发」：鼠标不动的时候一帧都不发（每拍仍会醒一次、做一次原子交换，
/// 那是可以忽略的开销）。位置没变时前端本来也不会看到任何区别（插值早就稳定在同一个点上）。
#[cfg(not(windows))]
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

        emit_mouse_move(
            &app_handle,
            f64::from_bits(MOUSE_X.load(Ordering::Relaxed)),
            f64::from_bits(MOUSE_Y.load(Ordering::Relaxed)),
        );
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

/// 一个鼠标坐标事件。
///
/// 形状必须和钩子那条路逐字一致（`{"x": f64, "y": f64}`）：前端 `useDevice.ts` 拿它算
/// 显示器比例，`usePairState.handlePointerMove` 拿它算「暂离后移动超过 4px 自动回来」。
fn emit_mouse_move<R: Runtime>(app: &AppHandle<R>, x: f64, y: f64) {
    let event = DeviceEvent {
        kind: DeviceEventKind::MouseMove,
        value: json!({ "x": x, "y": y }),
    };

    let _ = app.emit("device-changed", &event);
}

/// 一个鼠标按键边沿。**Windows 专用**（其它平台的按键事件照旧走钩子那条队列）。
///
/// 走和钩子那条路**同一个**构造函数 `discrete_device_event`：线上一字不差是它的职责，
/// 名字对不上不会报错，只会让对方的猫静默不亮那个键。这里也不需要 `platform_code`——
/// 键码（`KEY_CODES`）只对键盘有意义。鼠标事件只有这一条线程会发，所以顺序在这里就是全序。
#[cfg(windows)]
fn emit_mouse_button<R: Runtime>(app: &AppHandle<R>, button: rdev::Button, pressed: bool) {
    let event_type = if pressed {
        EventType::ButtonPress(button)
    } else {
        EventType::ButtonRelease(button)
    };

    let Some(event) = discrete_device_event(event_type) else {
        return;
    };

    let _ = app.emit("device-changed", &event);
}

/// Windows 的鼠标位姿采样器（见 `pointer.rs` 的模块注释）。
///
/// 「有变化才发」：鼠标不动的时候一帧都不发。位置没变时下游本来也不会看到任何区别
/// （本机那只猫的插值早就停在同一个点上；联机快照 `usePairState` 自己还有一层去重 + 限速）。
///
/// 「忽略鼠标事件」（`ignoreMouse`）开着时**照样采**：位姿在联机那侧还有第二个用途——
/// `usePairState.handlePointerMove` 拿它算「暂离后移动超过 4px 自动回来」，
/// 那条路今天就不看 `ignoreMouse`，跟着停掉采样会把它一起停掉。
/// 关掉「本机猫跟随鼠标」的那一步在前端（`useDevice` 的 `Ticker`），不在这一层。
#[cfg(windows)]
fn sample_pointer<R: Runtime>(app_handle: AppHandle<R>) {
    let interval = Duration::from_millis(1000 / MOUSE_EMIT_HZ);
    let mut edges = pointer::ButtonEdges::default();
    let mut last: Option<(f64, f64)> = None;

    loop {
        // 这条线程与进程同寿（装钩子那条线程是 `GetMessageA` 消息循环，不会返回），
        // 留个退出条件给以后可能加的「停止监听」——现在没人会把它置回 false。
        if !IS_LISTENING.load(Ordering::SeqCst) {
            return;
        }

        std::thread::sleep(interval);

        let Some((x, y)) = pointer::cursor_position() else {
            // 这一拍读不到光标（锁屏 / 切换桌面 / 会话切换）：**整拍跳过**，按键也不差分。
            // 那会儿 `GetAsyncKeyState` 同样不可信，拿它去差分只会凭空造出一串「抬起」，
            // 按着的键会假松开。顺手把 `last` 清掉，解锁后头一拍即使坐标没变也会补发一次位置。
            last = None;

            continue;
        };

        // 顺序写死：**先位置、后按键**。位置事件带的是「点下去那一刻光标在哪」，
        // 下游 `useDevice` 收到 `MouseMove` 就立刻开始本机插值、并把新比例喂给联机同步，
        // 所以新坐标要先交出去，再报这次按下 / 抬起。
        if last != Some((x, y)) {
            last = Some((x, y));

            emit_mouse_move(&app_handle, x, y);
        }

        let pressed_edges = edges.update(pointer::swapped_buttons(), pointer::button_state());

        for (button, pressed) in pressed_edges {
            emit_mouse_button(&app_handle, button, pressed);
        }
    }
}

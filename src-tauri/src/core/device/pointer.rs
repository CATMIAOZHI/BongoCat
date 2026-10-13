//! Windows 上「鼠标位姿」的**唯一**来源：按固定节拍**轮询**系统，而不是挂低层鼠标钩子。
//!
//! 为什么不能用 `WH_MOUSE_LL`：低层钩子的回调是在**系统输入的关键路径**上同步调用的
//! （rdev 先 `callback(event)`、之后才 `CallNextHookEx`），Windows 把输入交给前台之前
//! 必须等这个回调返回。回调本身已经不算慢了——上一轮优化把里面干的活砍到几次原子写——
//! 真正贵的是**系统为每一次鼠标移动都要唤醒并调度装钩子的那个线程**：1000Hz 的鼠标就是
//! 每秒上千次线程调度。表现是整台机器（不只是 BongoCat）鼠标发涩、而 CPU 并没有占满，
//! 花掉的是调度和唤醒，不是 CPU 时间。轮询没有这个代价：谁都不用等我们，一拍的固定开销
//! 就是一次 `GetPhysicalCursorPos` 加 5 次 `GetAsyncKeyState`。
//!
//! 代价有两个。一是位姿的最小延迟变成一拍（16ms，与本机 60fps 的平滑同一量级；旧路径那个
//! 合并线程本来也是在同一个 16ms 节拍上发「最新那一个坐标」，所以这里并不是多等一拍）。
//! 二是**边沿得自己算**：钩子能收到「刚按下」这个事件，轮询只看得到「现在按着」，
//! 所以由 [`ButtonEdges`] 在一拍与下一拍之间做差分。
//!
//! 自己算边沿带来两处与钩子不同的地方，都不是能顺手「修」掉的：按键 / 点击的延迟从「≈0」
//! 变成「0~一拍」（平均约半拍；`sleep` 的实际粒度让它最坏约 31ms），对猫爪动作和 60Hz 的
//! 联机快照无感；而同一枚键的一次完整按下 + 抬起如果整个落在同一拍里，会被整段丢掉——
//! 人手做不到 16ms 内完成一次点击，所以不为此加「补发」之类的机关。
//!
//! 轮询顺带把上游要专门处理的几件事（锁屏 / 切桌面、睡眠唤醒、设备被移除）变成了不需要处理：
//! 读的就是系统当下的真实状态。唯一要小心的是**读不到**的那一刻，见 [`cursor_position`]。

use std::os::raw::c_int;

use rdev::Button;
use winapi::shared::windef::POINT;
use winapi::um::winuser::{
    GetAsyncKeyState, GetPhysicalCursorPos, GetSystemMetrics, SM_SWAPBUTTON, VK_LBUTTON,
    VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};

/// 一拍里读几个鼠标键：左、右、中、X1、X2。下标与 [`BUTTON_VKS`] / [`buttons_for`] 对齐。
const BUTTON_COUNT: usize = 5;

/// 物理鼠标键的虚拟键码。**读的是物理键**，不受「切换主要和次要按钮」影响。
const BUTTON_VKS: [c_int; BUTTON_COUNT] =
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2];

/// 用户是不是把主要 / 次要鼠标键换了（`SM_SWAPBUTTON`）。
///
/// 每一拍读一次：一个 `GetSystemMetrics` 就是几条指令，相对 16ms 的节拍可以忽略，
/// 换来的是「改了就立刻生效」，不用管缓存什么时候失效。
pub fn swapped_buttons() -> bool {
    // SAFETY: 只读一个系统设置，没有前置条件。
    unsafe { GetSystemMetrics(SM_SWAPBUTTON) != 0 }
}

/// 一拍里 5 个鼠标键该报成什么名字，顺序就是 [`BUTTON_VKS`] 的顺序。
///
/// 名字必须和 rdev 在 `windows/common.rs` 里那套**逐字**一致（它是 `{:?}` 出来的）：
/// 前端 `useModel.ts` 只认 `Left`（别的都算右键参数），`usePairActivity.ts` 只认
/// `Left` / `Right`。名字一变不会报错，只会让对方的猫**静默**不亮那几个键。
///
/// 换了左右键之后两边口径会分岔，必须在这里对上：钩子报的是**逻辑**键——换了之后
/// 物理右键才是「主要键」，钩子把主要键报成 `WM_LBUTTONDOWN`，也就是 `Left`——
/// 而 `GetAsyncKeyState(VK_LBUTTON)` 读的是**物理**键，完全不受这个设置影响。
fn buttons_for(swapped: bool) -> [Button; BUTTON_COUNT] {
    [
        if swapped { Button::Right } else { Button::Left },
        if swapped { Button::Left } else { Button::Right },
        Button::Middle,
        // X1 / X2：钩子那边是 `WM_XBUTTONDOWN` 把 `HIWORD(mouseData)` 当 `Unknown(n)` 报的，
        // 1 和 2 就是从那儿来的。
        Button::Unknown(1),
        Button::Unknown(2),
    ]
}

/// 这一拍 5 个鼠标键的**物理**按下状态（下标对应 [`BUTTON_VKS`]）。
pub fn button_state() -> [bool; BUTTON_COUNT] {
    BUTTON_VKS.map(|vk| {
        // SAFETY: 只读系统状态，没有前置条件。
        // 最高位（`0x8000`）= 现在还按着；低位的「上次查询之后按过没有」我们不用。
        (unsafe { GetAsyncKeyState(vk) } as u32 & 0x8000) != 0
    })
}

/// 光标当前的**物理像素**位置，可能有负值（主显示器左边 / 上边那块屏就是负坐标）。
///
/// 用 `GetPhysicalCursorPos` 而不是 `GetCursorPos`：本进程是 per-monitor-v2 DPI aware
/// （tao 建事件循环时已经 `become_dpi_aware`），此刻两者返回值一致——但「谁先 DPI aware」
/// 是一种隐式顺序，太脆。钩子那条路上的 `MSLLHOOKSTRUCT.pt` 是物理坐标，前端
/// `utils/monitor.ts` 也按物理坐标比对显示器（只对 macOS 折算），口径必须一样。
///
/// 返回 `None` = 这一拍没读到（锁屏 / 切换桌面 / 会话切换）。**调用方必须整拍跳过**，
/// 按键也不能差分：那会儿 `GetAsyncKeyState` 同样不可信（当前桌面不是活动桌面时它一律
/// 返回 0），拿它去差分只会凭空造出一串「抬起」，按着的键会假松开。
pub fn cursor_position() -> Option<(f64, f64)> {
    let mut point = POINT { x: 0, y: 0 };

    // SAFETY: `point` 是本地变量、按 `LPPOINT` 交给系统；它只写这两个 `LONG`。
    let ok = unsafe { GetPhysicalCursorPos(&mut point) };

    (ok != 0).then_some((f64::from(point.x), f64::from(point.y)))
}

/// 「按下 / 抬起」的差分器：唯一需要跨拍记住状态的地方。
///
/// 钩子能告诉我们「刚刚按下了」，轮询只能告诉我们「现在按着」，所以边沿只能自己算。
/// 纯逻辑、不碰系统，好单测。
#[derive(Default)]
pub struct ButtonEdges {
    down: [bool; BUTTON_COUNT],
}

impl ButtonEdges {
    /// 交进这一拍读到的物理状态，吐出这一拍该报的边沿。
    ///
    /// 顺序写死成 [`BUTTON_VKS`] 的表顺序（左、右、中、X1、X2）：下游是按**每个键**各自
    /// 记状态的，同拍里两个键互不影响，但联机快照是在事件之间现取的，写死顺序才能让
    /// 「同一拍里谁先谁后」有唯一定义。
    ///
    /// 一个下标一拍最多出一个边沿（要么按下、要么抬起），所以不存在「同一个键同一拍
    /// 既按下又抬起」这种要额外定规矩的情况——16ms 之内做完一次完整点击不是人能做的事。
    pub fn update(&mut self, swapped: bool, now: [bool; BUTTON_COUNT]) -> Vec<(Button, bool)> {
        let buttons = buttons_for(swapped);
        let mut edges = Vec::new();

        for (index, pressed) in now.into_iter().enumerate() {
            if pressed == self.down[index] {
                continue;
            }

            self.down[index] = pressed;
            edges.push((buttons[index], pressed));
        }

        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只把指定的下标置为按下，其余当作没按。
    fn pressed_at(indices: &[usize]) -> [bool; BUTTON_COUNT] {
        let mut state = [false; BUTTON_COUNT];

        for &index in indices {
            state[index] = true;
        }

        state
    }

    /// 名字逐字钉住（见 [`buttons_for`]）：改名字不会报错，只会让对方猫静默失效。
    #[test]
    fn button_names_match_what_the_hook_used_to_report() {
        let names = |swapped| {
            buttons_for(swapped)
                .into_iter()
                .map(|button| format!("{button:?}"))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            names(false),
            ["Left", "Right", "Middle", "Unknown(1)", "Unknown(2)"]
        );
        // 换了左右键：物理右键才是「主要键」，钩子会把它报成 `Left`
        assert_eq!(
            names(true),
            ["Right", "Left", "Middle", "Unknown(1)", "Unknown(2)"]
        );
    }

    #[test]
    fn edges_fire_once_per_transition_and_never_on_steady_state() {
        let mut edges = ButtonEdges::default();

        // 左键按下：报一次
        assert_eq!(
            edges.update(false, pressed_at(&[0])),
            vec![(Button::Left, true)]
        );
        // 还按着：一次都不报（轮询最容易在这里刷屏）
        assert_eq!(edges.update(false, pressed_at(&[0])), vec![]);
        assert_eq!(edges.update(false, pressed_at(&[0])), vec![]);
        // 抬起：报一次
        assert_eq!(
            edges.update(false, pressed_at(&[])),
            vec![(Button::Left, false)]
        );
        // 仍然没按：一次都不报
        assert_eq!(edges.update(false, pressed_at(&[])), vec![]);
    }

    #[test]
    fn edges_in_one_tick_follow_the_table_order() {
        let mut edges = ButtonEdges::default();

        // 同一拍里中键和 X2 一起按下
        assert_eq!(
            edges.update(false, pressed_at(&[2, 4])),
            vec![(Button::Middle, true), (Button::Unknown(2), true)]
        );
        // 同一拍里一起抬起，顺序一样
        assert_eq!(
            edges.update(false, pressed_at(&[])),
            vec![(Button::Middle, false), (Button::Unknown(2), false)]
        );
    }

    #[test]
    fn swapped_buttons_report_the_logical_button() {
        let mut edges = ButtonEdges::default();

        // 换了左右键之后，物理右键（下标 1）才是用户眼里的「主要键」
        assert_eq!(
            edges.update(true, pressed_at(&[1])),
            vec![(Button::Left, true)]
        );
        assert_eq!(
            edges.update(true, pressed_at(&[])),
            vec![(Button::Left, false)]
        );
    }
}

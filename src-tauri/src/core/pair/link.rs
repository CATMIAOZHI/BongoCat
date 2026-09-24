//! P2P 传输的 cfg 中立门面（R24-1）。
//!
//! `manager.rs` 的调用点不该散着写 `#[cfg(windows)]`：这里在 Windows 上转发到真正的
//! [`super::p2p`]，其它平台给一份**同签名**的空实现。这样非 Windows 目标上 `live` 的
//! 调用点仍然会被编译，签名漂移能被抓到（客户端虽然只做 Windows，但仓库的
//! `release.yml` 仍然为 macOS / Linux 出包，那些目标不该因为 P2P 而编不过）。
//!
//! **注意谁来抓**：`client-ci.yml` 的 rust job 只有 `windows-latest`，所以 CI 上
//! stub 那一支**从不编译**。真正能发现签名漂移的是本机在非 Windows 目标上的
//! `cargo check`，以及打 `v*` 标签时 `release.yml` 的 macOS / ubuntu 任务。

#[cfg(windows)]
pub use super::p2p::{P2pEvent, P2pLink};
#[cfg(not(windows))]
pub use stub::{P2pEvent, P2pLink};

/// 可覆盖流（宠物快照、统计）可以走的第二条腿。
///
/// 抽成 trait 只为一件事：`flush_replaceable` 的**选路**要能被单测直接验证——真的
/// [`P2pLink`] 需要一条真的 DataChannel，而「按 kind 与两个标志选腿」这件事不该依赖它。
/// `Send + Sync`：它要在 `live` 的 `select!` 里跨 `await` 存活，而 `live` 跑在一个
/// `Send` 的 task 上。
pub trait CoverableLeg: Send + Sync {
    /// 发一帧。**失败即丢帧**：可覆盖流是绝对值快照，下一帧会盖掉它，所以这里不返回
    /// 错误、也不回队重发。
    fn send(&self, frame: Vec<u8>);
}

#[cfg(windows)]
impl CoverableLeg for P2pLink {
    fn send(&self, frame: Vec<u8>) {
        P2pLink::send(self, frame);
    }
}

/// 非 Windows 目标上这条腿永远不会 `open`（stub 不产生任何事件），所以这里什么都不做。
#[cfg(not(windows))]
impl CoverableLeg for P2pLink {
    fn send(&self, _frame: Vec<u8>) {}
}

/// 非 Windows 目标上的空实现：一条永远不会就绪、也不会回话的腿。
///
/// 存在的意义只有一个——让 `manager.rs` 里 `live` 的调用点保持 cfg 中立。
#[cfg(not(windows))]
mod stub {
    use super::super::protocol::{IceServer, PairSignalPayload};

    /// 与 `p2p::P2pEvent` 同形。这个平台上不会产生任何事件。
    #[derive(Debug)]
    pub enum P2pEvent {
        Signal(PairSignalPayload),
        Negotiating,
        Inbound(Vec<u8>),
        ChannelOpen,
        ChannelClosed,
    }

    pub struct P2pLink;

    pub struct P2pEvents;

    impl P2pEvents {
        /// 永不返回：调用点把它当成「这条腿不存在」。
        pub async fn next(&mut self) -> Option<P2pEvent> {
            std::future::pending::<Option<P2pEvent>>().await
        }
    }

    impl P2pLink {
        pub fn spawn(_device_id: String, _ice_servers: Vec<IceServer>) -> (Self, P2pEvents) {
            (Self, P2pEvents)
        }

        pub fn handle_signal(&self, _signal: PairSignalPayload) {}

        pub fn send(&self, _frame: Vec<u8>) {}
    }
}

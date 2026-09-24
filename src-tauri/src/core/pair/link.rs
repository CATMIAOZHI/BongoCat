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

/// DC 里的哪一条通道（§5.2 / R32）。
///
/// `Replaceable` 是 `ordered = false, maxRetransmits = 0` 的那条（宠物快照、统计），
/// `Reliable` 是有序有重传的那条（聊天、控制、附件分片）。两条都在同一条 SCTP 关联上，
/// 所以一条挂了另一条也挂了——但它们的**可用性标志各自独立**，因为每条通道都要有自己
/// 的「真的往返过一次」的证据才允许被使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Replaceable,
    Reliable,
}

/// 一次传输钉在哪条腿上（§8 Phase 10 的「传输在途时延后切换」）。
///
/// 附件分片是唯一「不能有缺口」的流（接收侧要求 seq 严格递增、V1 没有断点续传），
/// 所以它的 route 在 offer 时就定下来、这一单全程不改；聊天与 presence / 控制帧不钉，
/// 每帧按当前可用性选腿——两条腿两端都会读，只有分片有顺序约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Relay,
    Direct,
}

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
        P2pLink::send(self, Lane::Replaceable, frame);
    }
}

/// 非 Windows 目标上这条腿永远不会 `open`（stub 不产生任何事件），所以这里什么都不做。
#[cfg(not(windows))]
impl CoverableLeg for P2pLink {
    fn send(&self, _frame: Vec<u8>) {}
}

/// 可靠流（聊天、控制、附件分片）可以走的第二条腿（Phase 10）。
///
/// 和可覆盖流分开成两个 trait，是为了让两个**选腿**函数各自能被单测直接验证：
/// 它们的输入标志不同（可覆盖看 `pet-state` 通道，可靠看 `reliable` 通道）。
pub trait ReliableLeg: Send + Sync {
    /// 发一帧。**失败即丢**：这条腿不做重传，丢了靠中继兜底——聊天有 DB 的补发
    /// （`resend_pending_chat`）、在传的附件由 `direct_lost()` 按 §43 判失败。
    fn send(&self, frame: Vec<u8>);

    /// 这条腿现在还能不能收下一帧（DC 的发送缓冲有没有积压）。
    ///
    /// **必须是同步的布尔读**：`DataChannel::writable()` 是 async 的「等到有空间」，
    /// 在 `live` 的 `select!` 分支里 `await` 它会把中继腿的入站读取一起挡住。
    fn writable(&self) -> bool;
}

#[cfg(windows)]
impl ReliableLeg for P2pLink {
    fn send(&self, frame: Vec<u8>) {
        P2pLink::send(self, Lane::Reliable, frame);
    }

    fn writable(&self) -> bool {
        P2pLink::writable(self)
    }
}

/// 非 Windows 目标上这条腿永远不会 `open`，所以「不可写」是它的常态。
#[cfg(not(windows))]
impl ReliableLeg for P2pLink {
    fn send(&self, _frame: Vec<u8>) {}

    fn writable(&self) -> bool {
        false
    }
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
        Inbound(super::Lane, Vec<u8>),
        ChannelOpen(super::Lane),
        ChannelClosed(super::Lane),
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

        pub fn send(&self, _lane: super::Lane, _frame: Vec<u8>) {}

        pub fn writable(&self) -> bool {
            false
        }
    }
}

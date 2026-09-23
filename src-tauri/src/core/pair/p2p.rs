//! P2P（WebRTC）传输。
//!
//! 只在 Windows 上编译（`mod.rs` 里的 `#[cfg(windows)]`）：Phase 8 的范围就是
//! Windows 客户端，非 Windows 的 release 目标连 `webrtc` 那棵依赖树都不编译。
//!
//! 这里负责的是**传输本身**——`PeerConnection` 生命周期、ICE 配置、DataChannel；
//! 信令（`pair.signal`）走中继，仍然归 `manager.rs` 管。设计见
//! `docs/pair-plan-cloud-p2p.md` 的 §4 / §5 与修订记录 R21。
//!
//! 落地的顺序（R24-6）：先只把依赖接进来量一次编译时间与体积，再往上加代码。

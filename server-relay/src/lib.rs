//! 自建版 BongoCat 双人中继。
//!
//! 一对用户部署一个实例：它只做三件事——用派生出的 `PAIR_AUTH_TOKEN` 鉴权、
//! 维持两条 WebSocket（固定两个人）、在 A 与 B 之间转发帧并告知上下线。
//!
//! 它**不保存**聊天记录、图片、语音、文件与输入统计，也不解析应用负载——只读
//! 14 字节明文帧头做分桶限流。线上契约与 `server-cloudflare/` 完全一致：同一份
//! 客户端只改 URL 就能在两者之间切换。

pub mod auth;
pub mod http;
pub mod protocol;
pub mod relay;
pub mod server;

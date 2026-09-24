//! 自建版 BongoCat 双人中继。
//!
//! **一套服务器承载多个双人会话**（§2）：它只做四件事——按客户端给的
//! `ROOM_ID` + `AUTH_TOKEN` 摘要把连接划进各自的会话、维持每个会话两条 WebSocket
//! （每组固定两个人）、在同一个会话内转发帧并告知上下线、按 `PAIR_MAX_SESSIONS`
//! 限制同时存在的会话数。
//!
//! 服务器**没有**任何配对密码：Pair Secret 只存在于客户端，中继看到的是派生出来的
//! `ROOM_ID` 与 `PAIR_AUTH_TOKEN`，因此它既拿不到原始 secret，也拿不到 E2EE 根密钥。
//!
//! 它**不保存**聊天记录、图片、语音、文件与输入统计，也不解析应用负载——只读
//! 14 字节明文帧头做分桶限流。线上契约与 `server-cloudflare/` 完全一致：同一份
//! 客户端只改 URL 就能在两者之间切换（CF 版忽略 `X-Bongo-Room`，它一个部署仍然
//! 只服务一对用户，§17）。

pub mod auth;
pub mod http;
pub mod protocol;
pub mod relay;
pub mod server;

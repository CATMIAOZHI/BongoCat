# BongoCat 双人联机（第二阶段）设计：自建云服务器中继 + P2P（WebRTC）+ 60Hz

> ## 与 v1（`docs/pair-plan.md`）的关系
>
> - v1 = `docs/pair-plan.md`。Phase 1~6（多窗口置顶 / Cloudflare Relay / 对方猫 / 文字聊天 / 附件 / 语音）已实现、独立审计通过并推送，分支 `feat/pair-desktop-v1`。
> - v1 的线上契约是本计划的**不可动基线**：14 字节明文帧头（`kind|flags|transferId|seq`，帧头同时是 AEAD 的 associated data）、`AppEnvelope`、R17 的 HKDF 派生参数、关闭码、`server.welcome` / `server.peer` 的形状。改这些等于同时打破已部署的中继和对方的客户端。
> - 本文只描述**新增**能力：Phase 7 自建中继、Phase 8 P2P、Phase 9 60Hz。v1 的章节与 R1~R19 继续有效；两份文档冲突时以本文为准。
> - 需求来源：用户提供的「国内云 + P2P + 60Hz」建议对话。方向采纳，其中与现有实现不符的细节按修订记录修正。
> - 范围：客户端**只做 Windows**（与 v1 一致）；`server-relay/` 本身跨平台，但只承诺在 Linux + Docker 上跑通。

> ## 修订记录（R20~R24，实现前共识）
>
> 由主代理与只读评审 subagent 逐条讨论达成一致；评审结论为 `AGREED`，同时提出 4 处必须修正的细节，已并入下面各条（标注「评审修正」）。

> **R20（自建中继：契约逐条对齐，限流额度改为「广告」）**
>
> 新增 `server-relay/`：Rust + `tokio-tungstenite` + `tokio` 的单进程中继，实现与 Cloudflare 版**逐条对齐**的线上契约（要求的是行为一致，不要求实现方式或代码相同）：
>
> - `GET /health` → `{ok:true, protocol:1}`，**在鉴权之前返回**，因此**不能**放 TURN 凭据或任何秘密；
> - `GET /ws` 升级；三个头 `authorization: Bearer <token>` / `x-bongo-client: <deviceId>` / `x-bongo-protocol: 1`；
> - 客户端 → 服务端**只接受 binary**，收到 text 一律 `close 1008`（否则已配对的一方能伪造 `server.*`）；
> - 服务端 → 客户端只发 text + JSON；关闭码与阈值同 CF：`REPLACED 4002` / `PAIR_FULL 4003` / `STALE 4004` / `PROTOCOL_ERROR 1008` / `TOO_LARGE 1009` / `INTERNAL_ERROR 1011`，`STALE_AFTER_MS = 120_000`，最后活动时间最多每 10 秒写一次。
>
> **评审修正（限流不再是硬编码常量）**：CF 版把额度写死成 30 帧/秒、20 chunk/秒、12 MiB/秒；自建中继改成在 `server.welcome` 里**广告**：
>
> ```json
> {
>   "type": "server.welcome",
>   "protocol": 1,
>   "peerOnline": false,
>   "limits": { "framesPerSecond": 30.0, "chunksPerSecond": 20.0, "bytesPerSecond": 12582912.0 }
> }
> ```
>
> 客户端 Pacer 用广告值推导，**但不是 1:1 取用**：保持今天这组四元组不变（帧 20 速率 / 20 突发、分片 15 速率 / 10 突发，相对 CF 的 30 / 20 分别留 2/3、3/4、1/2 的余量），自建中继把额度调高时按同一比例放大；字段缺失时退回 30/20/12 MiB 的 CF 缺省值。`manager.rs:60-71` 的注释和 `manager.rs:3227-3232` 的测试都在守「严格小于中继上限」，1:1 取用会立刻复现 `close 1008`。这样自建中继可以放开到 60 帧/秒，而 CF 版拿到的缺省推导值与今天完全一致。`manager.rs` 的 `outbound_pacing_stays_within_the_relay_budget` 测试改为断言「缺省值仍在中继缺省额度内」，继续当护栏。
>
> **措辞订正（Phase 8 规划评审）**：上面那句「`outbound_pacing_stays_within_the_relay_budget` 测试改为断言『缺省值仍在中继缺省额度内』」与实现不符——那条测试**没有改**，它仍然硬编码中继的 30/20 当护栏；真正新增、守「缺省 = CF 推导」的是另一条 `the_default_pacing_is_the_cloudflare_derivation`。两条各守一件事，不是 bug。
>
> 部署产物：`Dockerfile` + `docker-compose.yml`（relay + Caddy 自动 TLS）+ 部署文档。文档必须写清：香港节点可避开大陆 ICP 备案（大陆节点依法需备案）；1 核 1GB / 2 Mbps 够用、5 Mbps 舒服；**客户端用 `rustls-tls-webpki-roots`，必须是域名 + 受信任证书，裸 IP 或自签证书连不上**。Cloudflare 版保留为「免费方案」。

> **R21（P2P：信令钉在中继上，两条腿各用各的心跳探针，能力门控对象是对端）**
>
> 本节的行号以 `2629fe2`（本计划定稿那版）为准。此后 `manager.rs` 又改过两次（`f99a862` 的额度参数化、Phase 8a 的 `live` 泛型化），行号已经整体漂移，**读代码时以函数名 / 注释定位**，不要按行号找。
>
> WebSocket 中继兼任信令通道，服务器只交换 SDP / ICE candidate。
>
> **评审修正 1（不新增帧 kind）**：中继会校验帧 kind，未知值直接 `close 1008`（`server-cloudflare/src/pair.ts:155-161`）。所以信令**复用 `FrameKind::Ping`(8)** 承载一个新的应用类型 `pair.signal`（kind 8 早就在中继的已知集合里，旧中继不会踢）。已部署的旧客户端收到未知应用类型**不会崩**——它落进 `_` 分支（`manager.rs:2509-2516`）；「忽略」这个词不准确，真实行为见 R26 第 5 条。
>
> **评审修正 2（心跳：DC 那条腿必须有自己的探针）**：DataChannel 上**不能用** WS `Message::Ping`——现有判定是 `awaiting_pong && last_inbound.elapsed() >= 2×心跳`（`manager.rs:1439-1445`），而 `awaiting_pong` 只在有入站消息时清除（`1385`）。只把适配器的 Ping/Pong 写成空操作、ticker 照旧发 `Message::Ping`（`1449`）是不行的：心跳根本没发出去，`awaiting_pong` 永远不为假，对端安静两分钟就命中超时判定，`run_session` 会重连整条会话并 `abort_transfers`（在传的附件被砍）。所以 **DC 那条腿**必须换成应用级 `pair.ping`。响应侧有现成实现：`protocol.rs:32` 的 `FrameKind::Ping`，收到就回 pong 在 `manager.rs:2249-2256`。立即失败条件 = 适配器流报错/结束、DC 或 ICE 进入 failed/closed、`send` 失败；心跳只做兜底。
>
> **但这不等于把中继那条腿也换掉。** 本条初版写的是「`1449` 的 ticker 改成入队一条应用级 `pair.ping` 帧（中继也照样转发，两端通用）」——**这条已被实测推翻，见 R28**：中继自己会回 WS Pong（`server-relay/src/relay.rs:322-324`），一旦改成对端回 pong，「对端离线」就等价于「心跳超时」，`stays_connected_across_heartbeats` 立刻红。
>
> **心跳归属（Phase 8 规划评审的定论，经 R28 订正）**：**两条腿各用各的探针，各自独立计时、独立标志**。§4.1 那个泛型参数是**中继这条腿**，DC 是 `live` 内部**多出来的第二条腿**（不是替换泛型参数），所以「中继腿的探针永远不变，只有 DC 腿会上下线」。约束：
>
> - **中继腿永远发 WS `Message::Ping`**（中继自己回 Pong，与对端在线与否无关；`server-relay/src/relay.rs:322-324`），**DC 腿发应用级 `pair.ping`**（DC 上没有 WS 控制帧）。这就是「心跳跟着传输走」的正确落点：不是「换掉发送点」，而是「每条腿用自己有的那种探针」。
> - **两条腿的标志不能共用**：中继腿用 `relay_awaiting_pong` / `relay_last_inbound`，**只被中继流的入站清除**；DC 腿用 `dc_awaiting_pong`，被 DC 入站清除（也允许被任何 `pair.pong` 清除，切换窗口里的竞态不该误判）。共用一个标志会造出「中继静默半死、却被 DC 的 pet-state 流量掩盖」的死角——今天只有一个传输所以撞不上，加了 DC 就会。
> - **超时的后果不同**：中继腿超时 → `Outcome::Lost("心跳超时")` → 重连整条会话；**DC 腿超时只把 active 切回中继，绝不返回 `Outcome::Lost`**（那会 `abort_transfers` 砍掉在传的附件，见修正 3）。
> - **中继腿的 WS Ping 不过 pacer、也不进 `state.reliable`**：它是 WS 控制帧，中继既不计桶（`relay.rs:324` 既不 `allow` 也不 `touch`）也不刷新 `last_seen`，过 pacer 只会白白吃掉一枚帧令牌；而 `state.reliable` 的上限是 512，队满时会挤掉最旧的聊天消息并把它退回 `pending`（`retry_dropped_chat`），等于每 60 秒一次系统性扰动。DC 腿的应用帧同理**不该吃中继的 pacer**——单一 pacer 会把 DC 上的 pet-state 压到 20 帧/秒，和 60Hz 的目标直接冲突（R23「帧额度随传输层走」要覆盖这一条）。
>
> **评审修正 3（切换必须在 `live` 内部完成）**：`run_session` 把 `live` 的**任何**退出都当成断线并 `abort_transfers`（`manager.rs:1116-1128`），所以不能靠「退出重进 `live`」来换传输，否则每次切换都会砍掉正在跑的附件传输。
>
> 推荐形状：`live` 继续**常读中继那条流**（`server.peer`、关闭码、信令都还在上面），只把「出站 sink」指向当前生效的传输；`SessionState`（帧序号、可靠/可覆盖队列、transfer 会话）全程不重置。信令**永远钉在中继**，重新协商前先把 active 切回中继（否则 DC 一断就没人送 SDP）。
>
> 能力门控：**只门控对端**——双方互发应用级 hello 声明 P2P 能力，只有收到对端声明才开始 ICE，否则对面是旧客户端时会白等一轮超时。`pair.signal` 走的是 kind 8，旧中继本来就原样转发，所以**不把「中继广告」当成 P2P 的前提**：`server.welcome` 里的 `limits`、`iceServers`、`capabilities` 都是可选的顶层增强字段——自建中继目前一定带 `limits`、配了 coturn 才带 `iceServers`，`capabilities` 留到 Phase 8 加 P2P 时再广告；缺失就按缺省值走。`server-cloudflare` 本次同步补这些可选字段**不是阻塞项**（列为 Phase 7b 的可选项）；没补的时候 CF 用户要在设置页手填 STUN/TURN，否则只有 host candidate（同局域网可用）。
>
> 回落策略：中继**一直保持**（它本身就是信令通道 + 离线检测，成本只是一枚 60 秒心跳），不做「失败再回落」。并行推进：一连上中继就是可用状态、状态立刻从中继发；ICE 在后台同时跑；只有 DC `open` 且应用级 ping/pong 往返成功，才把**可覆盖流**切过去。ICE 那 10~30 秒对用户不可见。切换按消息边界进行，**绝不在传输中途切**（V1 没有断点续传，接收侧要求分片序号严格递增，`transfer.rs:306-317`）；DC 中途掉线立刻回中继，当前附件按 §43 判失败。
>
> 第一阶段（Phase 8）**只让 DC 承载可覆盖流（pet state / stats）**，附件分片、聊天、控制、信令、重连留在中继；**心跳是唯一的例外——两条腿各自发**（见上面的「心跳归属」与 R28）。**附件分片绝不能走 `pet-state` 通道**（它是 `ordered=false, maxRetransmits=0`，而接收侧要求分片序号严格递增、V1 没有断点续传，丢一片或乱一片整单就废）；将来要让传输走 P2P，只能走 `reliable` 通道。这样范围比「全量替换」小得多。
>
> STUN/TURN 来源 = `server.welcome` 广告 + 设置项覆盖。固定双人用**静态凭据**可接受（coturn `lt-cred-mech`，一对用户名密码只经已鉴权的 welcome 下发）；更严就用 `use-auth-secret`（username = 过期时间戳，password = `base64(hmac-sha1(secret, username))`）。TURN 建议同时开 UDP 与 TCP/443（部分运营商/企业网封 UDP）；coturn 的 TLS 证书要单独配，Caddy 只管 HTTP。
>
> 隐私：README 承诺「不收集任何用户数据」，而 STUN 必然让第三方看到公网 IP——这是 WebRTC 固有的。**默认不填**任何公共 STUN，并在设置页写明；中继广告的 STUN/TURN 是可选的。
>
> 文案：`describe_close`（`manager.rs:1460-1471`）与 `client.rs:127-131` 里所有「中继…」措辞要改成中性说法（P2P 下同样会命中）。

> **R22（分片大小随传输层走；接收侧必须真的用它）**
>
> **评审修正 4**：接收侧现在**根本不看** offer 里的 `chunk_size`——它硬性要求 `payload.chunk_size == CHUNK_SIZE`，不等就报「附件 offer 的参数不合法」（`manager.rs:1971-1972`），收分片时也按本地常量重算（`transfer.rs:319`），`IncomingTransfer` 结构体里压根没有 `chunk_size` 字段（`transfer.rs:267-276`）。所以「协议不用改」成立，但**实现要改**：
>
> - `transfer.rs`：`chunk_count` / `chunk_length` 加参数（`40-53`）；`OutgoingTransfer` 存 `chunk_size` 并用它 seek（`191-256`，seek 在 `247`）；`IncomingTransfer` 存并用于分片长度（`267-341`，用在 `319`）；
> - `manager.rs`：`1757`（发送侧）、`1971-1976`（接收侧校验改成「用 offer 的值 + 夹紧」）、`2044`（会话记录）；
> - 测试里 8 处直接用 `CHUNK_SIZE` 的地方（`manager.rs:3619` / `3669` / `3689` / `3690` / `3845` / `3917` / `4271` / `4281`）与 3 处用 `chunk_count(size)` 的（`4003` / `4019` / `4091`）都要跟着加参数。
>
> 夹紧规则：上界 ≤ 512 KiB **且** ≤ `MAX_BINARY_FRAME_SIZE - 14 - 24 - 16`；**加下界**（如 ≥ 4 KiB），否则对端把 `chunk_size` 报成 1 就能逼你在本地写十亿次小文件。`chunk_size` 在 AEAD 覆盖范围内、只有配对端能改，所以这是防呆，不是外部攻击面。
>
> P2P 下用更小值（48 KiB 保守默认）；中继下保持 512 KiB。
>
> **理由订正（Phase 8 规划评审）**：原先写的「SCTP 默认消息上限 64 KiB，RFC 8841」在本实现里不成立，48 KiB 这个**结论**保留、**理由**换掉。真实情况：`webrtc` 0.21 的 SCTP 上限是 `MAX_MESSAGE_SIZE = 262144`（256 KiB，`rtc-0.21.0/src/peer_connection/configuration/setting_engine.rs`），只有超过它才报 `ErrOutboundPacketTooLarge`（`rtc-sctp-0.21.0/src/association/stream.rs`），**256 KiB 以内由实现自己分片**；`DataChannelEvent::OnMessage` 文档里那句「最多 16384 字节」在整个 crate 里只出现一次、是抄自 W3C 的旧文案，没有任何代码常量或校验支撑，它提到的 "detach API" 在 0.21 里也不存在。所以不必把 P2P 分片降到 12 KiB。夹紧上界（≤ `MAX_BINARY_FRAME_SIZE - 14 - 24 - 16`）继续保留——DC 路径的帧同样要过 `SessionState::encode` 的 1 MiB 检查。
>
> **连带修正**：Pacer 也是按**块/秒**算的（`manager.rs:70-71`，15 块/秒），48 KiB × 15 ≈ 720 KiB/s，比现在慢一个数量级。所以分片额度要么改成按字节，要么和传输层一起参数化。

> **R23（60Hz 上限 + 远端插值）**
>
> 60Hz 的生效条件是**当前生效传输的额度**：P2P 下没有中继额度约束；自建中继只要广告的额度够也能跑；CF 中继的令牌桶是每 socket 30 帧/秒（`protocol.ts:59` + `pair.ts:165`），实际落在从广告值推导出来的速率（约 20 帧/秒）。任何情况下都保持「变化才发、空闲不发」。
>
> **评审修正**：Rust 侧 Pacer 是硬约束（`manager.rs:63-64` = 20 帧/秒），而且有测试把这个假设写死了（`manager.rs:3220-3233`）。60Hz 必须让帧额度与分片额度都随传输层走（配合 R20 的广告额度），否则中继收 30、客户端反而更保守。
>
> 带宽不用担心：量化本身就是天然限流（`POINTER_STEP = 0.02`，`usePairActivity.ts:46`，1000 px/s 的鼠标每秒只产生约 26 个不同的 x 值；强度步长 0.2、左右手是布尔）。60Hz 只是**上限**，实际远低于它。
>
> **评审修正（前端）**：remote-cat 的 `DECAY_INTERVAL_MS = 200`（`src/pages/remote-cat/index.vue:44`），现在快照来了只是存进 `ref`，模型每 200ms 才更新一次。**只提频率、不做插值，视觉上不会有任何变化**——所以「远端本地插值」是前提条件，不是可选项。
>
> 不要用 16ms `setInterval` 驱动发送（Windows 默认定时器精度 15.6ms）；保持现有的事件驱动 + 尾随定时器（`usePairState.ts:104-115`）。
>
> 入站 pet-state 只 emit 事件、不碰数据库（`manager.rs:2276-2288`），所以 60Hz 对 SQLite 没有压力。

> **R24（工程前置：CI、编译 spike、证书）—— 评审提出的漏项，按风险排序**
>
> 1. `webrtc` crate（0.21）必须在 `.github/workflows/release.yml` 的 **3 个 Windows 目标**上编译通过：`x86_64-pc-windows-msvc`、`i686-pc-windows-msvc`、`aarch64-pc-windows-msvc`。**先做一个只验证编译的 spike**，这是最大的进度风险。
>
>    **订正（Phase 8 规划评审）**：原文写的是「全部 7 个目标」。范围本来就是「客户端只做 Windows」，所以依赖用 `[target.'cfg(windows)'.dependencies]` 门控、`p2p` 模块用 `#[cfg(windows)]` 门控，另外 4 个非 Windows 目标（`macos-latest` × 2、`ubuntu-22.04`、`ubuntu-22.04-arm`）**完全不编译 `webrtc`**，风险面从 7 个降到 3 个。形状定死：
>    - 依赖写 `webrtc = { version = "0.21", default-features = false, features = ["runtime-tokio", "crypto-ring"] }`（0.21 的默认就是这两个，显式写出来是为了抗上游改默认），放在根 `Cargo.toml` 的 `[workspace.dependencies]`，`src-tauri` 用 `webrtc.workspace = true`；
>    - 门控挂在 `src-tauri/src/core/pair/mod.rs` 的 `#[cfg(windows)] pub mod p2p;` 那一行，不要在 `p2p.rs` 里再写一次 `#![cfg(windows)]`；
>    - 调用点**不要**在 `manager.rs` 里散着写 `#[cfg(windows)]`。新开 `pair/link.rs` 做一层 cfg 中立的 seam：`#[cfg(windows)]` 一份真实现、`#[cfg(not(windows))]` 一份空实现。这样非 Windows 目标上 `live` 的调用点仍然会被编译，签名漂移能被 CI 抓到；
>    - 不要改用 cargo feature 代替 `cfg`：feature 是全局加法式的，而 `release.yml` 的 `args` 是各目标共用的，改成按目标传参容易漏，`cfg(windows)` 漏不了。
>
>    **spike 结果（已跑完）**：`x86_64-pc-windows-msvc` 编译 + 实跑双 peer 互连通过；`i686-pc-windows-msvc` `cargo check` 通过；`aarch64-pc-windows-msvc` 本机因缺 `clang` 无法自证，但唯一需要 C 工具链的 `ring 0.17.14` **本来就在依赖树里**（`rustls` ← `reqwest`/`hyper-rustls` ← `tauri-plugin-updater`，以及 `tokio-tungstenite`），上游 `ayangweb/BongoCat` 的 release 在 `windows-latest` 上跑 aarch64-windows 是 success，所以没有引入新的构建工具要求。**注意**：本 fork 至今没跑过 `release.yml`（0 个 tag），想要本仓库的真实记录，`workflow_dispatch` 手动跑一次最便宜（会产出 Draft Release）。另：spike 的 ICE 证据只覆盖 loopback（绑定的是 `127.0.0.1:0`），**生产绝不能绑回环**——要么不设 `with_udp_addrs` 走缺省、要么绑通配地址，否则收不到真实 host 候选。
>
> 2. 仓库**没有 PR / 提交级别的编译或测试门禁**（`.github/workflows` 只有 `release.yml` / `sync-to-gitee.yml` / `upgradelink.yml`；`release.yml` 只在打 `v*` tag 时 `pnpm tauri build`，会编译但不跑测试）。Phase 8 是大重构，没有 CI 兜底很难受——先补一个跑 `cargo test --lib` + `pnpm test` + `tsc --noEmit` 的 job。（`server-relay/` 那一份已在 R25-7 落地，客户端侧仍留到 Phase 8。）
> 3. 自建中继必须有受信任证书（客户端用 `rustls-tls-webpki-roots`）：文档强制要求域名；「允许自签 / 自定义 CA」作为可选设置项，默认不做。
> 4. R10 的 secret 纪律要覆盖自建流程：compose 用 env 文件或 stdin 喂，**不要**写 `-e PAIR_AUTH_TOKEN=xxx`；需要一个与 `server-cloudflare/scripts/generate-pair.mjs` HKDF 等价的本地生成器（自建侧用户不装 Node）。
> 5. 香港节点避备案是对的，但 UDP 可能被封、跨境链路抖动大——这恰恰是 P2P 收益最大的场景，TURN 要备 TCP/443。
> 6. `webrtc` 会明显拉长编译时间与产物体积（3 个 Windows 目标）——**依赖落地那一刻量**：8b 的第一个提交只加依赖 + 空 `p2p.rs` + cfg 门控，先量再决定要不要进一步按 feature 门控。量法（Windows 本机、同 profile 同 target）：编译时间用 `cargo build --release --target x86_64-pc-windows-msvc --timings`（基线用当前 HEAD 量一次，重点看总墙钟和 `rtc-*` / `ring` / `rkyv` / `rcgen` 的占比）；体积看 `target/release/bongo-cat.exe` 与 `pnpm tauri build` 出来的 `bundle/nsis/*.exe`。规模参考（评审已量）：净新增 41 个 crate 名（`rtc-*` 家族 15 个，加 `rcgen`/`x509-parser`/`der-parser`/`asn1-rs`/`pem`/`yasna`、`rkyv`/`bytecheck`/`ptr_meta`/`rend`/`rancor`/`munge`、`quinn-udp`/`crc32c`/`crc`/`event-listener`/`sansio`/`unicase`/`ccm`/`ctr`/`md-5` 等），`bytes` 会被顶到 1.12.x。注意根 `Cargo.toml` 的 `[profile.release]` 是 `panic=abort` + `lto=true` + `codegen-units=1`，链接阶段超线性，成本主要体现在 fat LTO，别只看 `cargo check` 的时间。

> **R25（Phase 7 实现评审的收口）—— 只读审计提出的 1 个 P1 与 14 个 P2，实现阶段已逐条处理**
>
> 1. **摘牌必须真的关连接（P1）**：`forward` 转发超时后原先只把对端移出注册表，socket 还挂着——注册表说它离线，它却还能把帧转给对方，两边都不会自愈。现在每条连接带一个 `oneshot` 摘牌信号（注册表里那份 `Sender` 一被丢弃，读循环就退出），连接收尾时再给 writer 一点时间把排队的关闭帧发完（`WRITER_DRAIN_TIMEOUT` 5 秒），发不完就中止任务，让 socket 真正关闭。
> 2. **控制帧投不进去也要摘牌**：上下线公告走 `try_send`，队列满说明那条连接已经停摆；原先静默丢掉会让另一方永远以为它在线。现在按同一套逻辑把它摘掉，并补一条离线公告给剩下的人。
> 3. **限流额度按字段判定 + 设上限**：三个维度各自独立（某个字段坏掉只让它退回 CF 缺省，不会连带丢掉另外两维），并夹在 帧/分片 ≤ 240、字节 ≤ 64 MiB 以内——中继是对方维护的，不能由它把客户端速率推成任意高。客户端只消费帧与分片两维（字节维度是中继自己的桶，README 已写明）。
> 4. **补发前必须先拿到 welcome**：客户端在读到 `server.welcome` 之前不发送任何应用帧（`read_welcome`，超时 10 秒）。原先第一次补发用的是缺省 20/20，撞上一个广告额度更低的自建中继会直接被 `close 1008`。
> 5. **WebSocket 层上限放到 8 MiB**：1 MiB ~ 8 MiB 的帧能被完整读完、干净地回 `1009`；超过 8 MiB 时中继在读帧头时就拒绝，帧体还留在接收缓冲里，关连接会让 TCP 发 RST（对端看到「连接被重置」）。两种结果对客户端是同一件事：重连。
> 6. **握手也要校验与超时**：读请求头有 10 秒超时；`Sec-WebSocket-Version` 必须是 13（否则 426 + `Sec-WebSocket-Version: 13` 头），`Sec-WebSocket-Key` 必须是 16 字节的 base64（否则 400）。解析错误串里不再回显请求头原文——漏了冒号的 `Authorization` 不会把凭据带进日志。
> 7. **CI**：新增 `.github/workflows/pair-relay.yml`，对 `server-relay/` 跑 `cargo fmt --check` / `cargo clippy -D warnings` / `cargo test`。
> 8. **顶替时的离线通知对齐 CF**：`4002`（同一 deviceId 重连）**不发**离线通知（CF 的 `announceOffline` 按 deviceId 过滤掉了）；`4004`（陈旧顶替）**要发**——CF 靠被顶替连接的 close 事件补，自建版显式补，并放在新连接上线之前，存活方的帧序列是「旧的离线 → 新的上线」，终态是在线。两处有意的差别（README 已写明）：CF 那条离线帧是在新连接被接受之后才发的，所以**新连接自己**也会收到、反而显示「对方离线」；自建版只发给多出来的那一方。另外 `admit` 的 `peerOnline` 仍沿用「动手之前算好的 remaining」（与 CF 一致）：如果存活方恰好在那一刻停摆，它会在广播里被摘掉，于是新连接的 welcome 会说 `peerOnline: true` 而注册表里其实只剩它自己——窗口很窄（要求存活方队列满），且对方重连时会再发一次上线通知，自愈，所以按评审建议保留现状。`bytesPerSecond` 是广告但不被客户端消费（README 已写明）。

---

> **R26（Phase 8 规划评审的收口）—— 只读评审 subagent 的结论是 `CHANGES REQUIRED`，5 处必改已全部并入上面各条**
>
> 评审确认「按原样做」的结论，记在这里备查：
>
> 1. **`webrtc` 依赖门控**：`[target.'cfg(windows)'.dependencies]` + `#[cfg(windows)]`，非 Windows 的 4 个目标完全不编译 `webrtc`；形状细节见 R24-1。
> 2. **Phase 8a 的形状**：`live` 的泛型化只需改签名与 `transport.split()` 一行，四个辅助函数（`flush` / `enqueue_reply` / `send_next_chunk` / `send_frame`）与 `read_welcome` 的约束天然兼容，`SessionState` 不碰 socket，调用点只有 `run_session` 一处。**不需要 WS 适配器**（见 §4.1 的订正）。
> 3. **`pair.signal` 走 `FrameKind::Ping`(8) 的尺寸安全**：中继侧 `is_known_frame_kind` 是 `(1..=8)`、没有按 kind 的尺寸限制，唯一上限是 1 MiB（`MAX_BINARY_FRAME_SIZE`）；CF 侧同理（`pair.ts:157` 判定、`143` 上限，R21 修正 1 引的 `155-161` 准确）；客户端入站只在 Binary 分支查 1 MiB。固定开销 = 14 帧头 + 24 nonce + 16 tag，信封 JSON 约 95 字节；700 字节的 SDP 整帧约 850 字节，非 trickle 把 6 个候选全塞进 SDP 也就约 1.5 KiB，相对 1 MiB 有约 700 倍余量。kind 8 同时承载信令与应用级心跳，靠信封里的 `message_type` 区分，互不冲突；分片桶只对 kind 6 计数，所以 kind 8 **不占分片桶**（帧桶与字节桶照常计，信令只有个位数帧，可忽略）。**R28 订正后中继腿上已经没有应用级心跳**（中继腿是 WS Ping），走中继的 kind 8 只剩信令；DC 腿的 `pair.ping` 根本不经过中继。
> 4. **依赖兼容性（逐条核过）**：`tokio` 要求 `^1.52.3`、仓库锁 1.53.1 ✓；`rustls` 要求 `^0.23.27`、仓库锁 0.23.38 ✓ 且与 `tokio-tungstenite 0.30` 共用一份；`tokio-tungstenite` 在 `rtc` 里只是 dev-dependency 0.28，**不进生产依赖图**；`ring` 要求 0.17.14、仓库锁正好 0.17.14 → 不新增版本、不新增构建工具要求。`async-trait` 是**新的直接依赖**（`PeerConnectionEventHandler` 的 impl 必须挂 `#[async_trait::async_trait]`），但 0.1.89 已在 lock 里当传递依赖，加它不会动 lock。其余新增全是纯 Rust，没有 openssl / native-tls。
> 5. **老客户端收到 `pair.signal` 的真实行为**：不是「忽略」，而是落进 `_` 分支 `emit(EVENT_MESSAGE)`，把整个信封（含 SDP）广播给所有 WebView；因为 `pair-message` 这个事件常量没有任何前端订阅者，实际是 no-op。结论不变（老客户端不会崩），但能力门控仍然必须做——否则对面是旧客户端时我们会白等一轮 ICE 超时。

> **R27（Phase 8 起步落地：传输抽象、依赖门控、CI）—— 含只读审计提出的 2 个 P0**
>
> 1. **Phase 8a 已落地（`c975786`，零行为变化）**：`live` 泛型化为 `T: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin, E: Display`，函数体只改 `transport.split()` 一行。四个辅助函数（`flush` / `enqueue_reply` / `send_next_chunk` / `send_frame`）与 `read_welcome` 的约束天然兼容，`SessionState` 不碰 socket，调用点只有 `run_session` 一处。**不需要 WS 适配器**：`WebSocketStream` 本来就同时满足两个 bound 且两侧同一个 `Error`。测试加了 `FakeTransport`（非 WebSocket 的传输替身）+ `wait_until`，用例 `live_runs_over_a_transport_that_is_not_a_websocket`。验证：`cargo test --lib` = 104 passed / 6 ignored；真中继 e2e 5/5。
> 2. **`webrtc` 依赖门控已落地**：根 `Cargo.toml` 的 `[workspace.dependencies]` 加 `webrtc = { version = "0.21", default-features = false, features = ["runtime-tokio", "crypto-ring"] }`（0.21 的默认就是这两个，显式写是为了抗上游改默认）与 `async-trait`；`src-tauri/Cargo.toml` 放 `[target."cfg(windows)".dependencies]`；`src-tauri/src/core/pair/mod.rs` 加 `#[cfg(windows)] pub mod p2p;`。R24-1 的 spike 已跑完：`x86_64-pc-windows-msvc` 编译 + 双 peer 实跑互连通过（`connected` / DC `open` / SCTP 协商成功 / 收到 15 字节），`i686-pc-windows-msvc` `cargo check` 通过，`aarch64-pc-windows-msvc` 本机缺 `clang` 无法自证（唯一需要 C 工具链的 `ring 0.17.14` 本来就在依赖树里，上游 aarch64-windows release 是 success）。
> 3. **R24-6 的测量结果（已量）**：`cargo build --release` 总墙钟 **5 分 23 秒**（323.4 s）；`target/release/bongo-cat.exe` = **12,761,088 字节（约 12.2 MiB）**；`Cargo.lock` 净新增 **41 个 crate 名**（`rtc-*` 家族、`rcgen`/`x509-parser`/`der-parser`/`asn1-rs`/`pem`/`yasna`、`crc`/`crc32c`/`ccm`/`ctr`/`md-5`、`quinn-udp`/`sansio`/`unicase`/`substring`/`minimal-lexical`/`oid-registry`/`rusticata-macros`/`munge`/`rancor` 等）。**没有 HEAD 基线**：本 fork 至今 0 个 tag、没跑过 `release.yml`，所以这是有依赖的绝对值而非增量，判为可接受；要不要进一步按 feature 门控，等 8b/8c 写完再看。
> 4. **CI job 已落地（`.github/workflows/client-ci.yml`，先提交为 `1f4c198`，随后按审计意见修正）**。只读审计对 `1f4c198` 给出 `P0×2 + P1×1 + P2×3`，其中两个 P0 是「照着规划评审的建议写就会踩」的坑：
>    - **P0-1**：规划评审建议的 `node-version: 20` 与 pnpm 11 不兼容。pnpm 11 用了 `node:sqlite`，在 Node 20 上连 `pnpm -v` 都崩 → 两个 job 都改 `node-version: 24`（并统一 `pnpm/action-setup@v4`、`version: 11`）。
>    - **P0-2**：rust job 在干净检出上跑不起来。`tauri-build` 在 Windows 上要读 `icons/icon.ico` 生成资源文件，而 `src-tauri/icons` 不入库 → 必须在 `cargo test` 之前补 `pnpm install` + `pnpm build:icon` 两步。
>    - **P1-1**：§11 那段「CI job 的形状」必须按实际落地的样子改（已改，见 §11）。
>    - **P2**：加 `permissions: contents: read`；写明覆盖边界（`cargo test --lib` 不含 bin target、`tsc` 不含 `.vue`）；`paths` 去掉 `vite.config.ts`。
> 5. **pnpm 11 的 `allowBuilds`（修 P0 时发现的深层问题）**：pnpm 11 默认拒绝执行依赖的 build script，只要有一条被忽略就让 `pnpm install` 以 `ERR_PNPM_IGNORED_BUILDS` 退出 1；更麻烦的是 `pnpm run` 之前那次依赖检查会**再跑一次 install**（`verify-deps-before-run` 默认值 `install`），命令行上的 `--config.strict-dep-builds=false` 传不进那一次（`PNPM_CONFIG_STRICT_DEP_BUILDS=false` 这种带 `PNPM_CONFIG_` 前缀的环境变量能穿进去，但每条命令、每个 CI step 都得带，比 `allowBuilds` 脆得多），于是 `pnpm test`、`pnpm build:icon`、`pnpm exec vitest run` 会跟着一起红。唯一的干净修法是仓库根新增 `pnpm-workspace.yaml` 并声明 `allowBuilds: { '@parcel/watcher': true, esbuild: true, simple-git-hooks: true }`（这正是 pnpm 11 自己写占位内容时用的键；只放占位字符串值不管用）。实测：`pnpm install --frozen-lockfile` → 0；`pnpm test` → 51 tests 通过；`pnpm build:icon` → 0 且生成 `icon.ico`；`tsc --noEmit` → 0。副作用是本地 `simple-git-hooks` 会被真装上，从此本机每次 commit 都走 `npx lint-staged`（= 对 staged 文件跑 `eslint --fix`）+ `commitlint -e`；CI 不受影响（CI 里不会触发 pre-commit）。
> 6. **CI 门禁已在真 runner 上跑绿**：`web` 24 秒、`rust` 6 分 41 秒（`feat/pair-desktop-v1` 的 push 运行）。这个 gate 的**首次 runner 运行即通过**——在此之前它只有本机实测，没有跑过 GitHub runner。

---

> **R28（心跳探针订正：两条腿各用各的，中继腿永远是 WS Ping）—— 实测推翻了 R21 的初版写法**
>
> 1. **被推翻的内容**：R21「评审修正 2 / 心跳归属」初版要求把 ticker 分支从 WS `Message::Ping` 换成应用级 `pair.ping`，理由是「中继也照样转发，两端通用」。**这个理由只对 DC 成立**：中继那条路的 WS Ping 本来有人回答——中继自己回 Pong（`server-relay/src/relay.rs:322-324` 明写「Ping 由 tungstenite 在读循环里自动回 Pong……这里再发一条会让对端收到两个 pong」），e2e 的 `heard_pong`（`e2e.rs:715-727`）守的就是这条契约。
> 2. **实测证据**：按初版改完后单元测试全绿（105 passed / 6 ignored，含新增的 `the_heartbeat_is_an_application_ping_frame`），但真中继 e2e 立刻红：
>
>    ```text
>    test core::pair::e2e::stays_connected_across_heartbeats ... FAILED
>      panicked at e2e.rs:738:5: 心跳期间发生了重连:
>      [Connecting, Connecting, ConnectedPeerOffline, ConnectedPeerOffline,
>       Connected, Connected, Reconnecting, Connecting, ConnectedPeerOffline, Connected]
>    ```
>
>    根因：该用例的 A 端**对端离线**。中继只**转发** `pair.ping`，回 pong 的是对端；对端离线 → 没有 pong → `awaiting_pong` 永远为真 → 4 秒（2×心跳）后判「心跳超时」→ `run_session` 重连整条会话。**心跳从「探测中继/TCP 是否活着」变成了「探测对端是否活着」**，而对端离线时后者必然为假，于是退化成每 4 秒一次的重连抖动。同一个根因还连累了 `two_clients_exchange_a_file_through_the_relay`（抖动的客户端一直占着配对位，报 `4003 配对已满`）。回退这次改动后 e2e 立刻回到 **5 passed / 0 failed**。
>
> 3. **订正后的形状**：两条腿各用自己有的探针——**中继腿永远发 WS `Message::Ping`**（中继回 Pong，与对端在线与否无关；不过 pacer、不进 `state.reliable`），**DC 腿发应用级 `pair.ping`**（DC 上没有 WS 控制帧）。两个标志独立：`relay_awaiting_pong` 只被中继入站清除、超时 → 重连；`dc_awaiting_pong` 被 DC 入站（或任何 `pair.pong`）清除、超时 → **只回落中继，绝不返回 `Outcome::Lost`**。这样「对端离线」只影响 `server.peer` 的 `peerOnline` 与 UI，不再触碰心跳判定。**后续指针**：等 8c 的 DC 腿落地后，`send_frame` 上方那句「所有**应用帧**都必须先过 `Pacer::acquire`」就不再普遍成立（DC 帧不吃中继 pacer），届时要说清是「**走中继的**应用帧」——R23 把额度按传输层拆开时本来就会碰到这里。
> 4. **落地时机**：心跳改动**不放进 Phase 8a**（8a 的承诺是零行为变化，而这是一次行为变更），也不单独提交——它和需要它的 DC 适配器一起在 **8b/8c** 落。中继腿在当前状态下**不需要任何改动**（今天就是 WS Ping），所以 8b/8c 之前 `manager.rs` 的 ticker 分支保持原样。
> 5. **顺带查清的两个事实**（都不构成回归，记下来免得再查一遍）：
>    - **空闲不会被主动踢**：自建侧 `stale_after` 的缺省是 `DEFAULT_STALE_AFTER_MS = 120_000`（`server-relay/src/protocol.rs:47`，运行期字段叫 `stale_after`），CF 侧是 `STALE_AFTER_MS = 120 * 1000`（`server-cloudflare/src/protocol.ts:68`）；但**只在 `admit()` 里判定一次**（`relay.rs:375` / `pair.ts:65-69`），没有后台清理任务。所以 DC 活跃期间完全不发 WS Ping 也不会被 4004 顶替。
>    - **`touch()` 只在 Binary 帧上调用**（`relay.rs:307`；`pair.ts:169` 同理），WS Ping/Pong 既不计桶也不刷新 `last_seen`。所以「应用级心跳会顺带刷新 `last_seen`」是初版唯一真实的好处，但代价是上面那个致命误判，不划算；今天空闲超过 120 秒后重连会拿到 4004 而不是 4002，这是**既有行为**，本次不改。

> **R29（Phase 8b 落地：`pair.signal` + ICE + `pet-state` DataChannel）—— 提交 `f3427cc`**
>
> 1. **信令走 `FrameKind::Ping`(8)**：`protocol.rs` 新增 `message_type::SIGNAL = "pair.signal"`、`SIGNAL_VERSION = 1` 与 `PairSignalPayload`（`#[serde(tag = "kind", rename_all = "lowercase")]` 的 `hello` / `offer` / `answer` / `candidate`，字段 camelCase）。`hello` 带 `version` 与 `deviceId`；`offer` / `answer` 的 `description` 是**序列化后的 `RTCSessionDescription`**（含 SDP 文本与类型，不是裸 SDP）。不新增帧 kind（R21 修正 1）：kind 8 早在中继的已知集合里，旧中继照样转发。
> 2. **ICE 服务器由中继广告**：`ServerFrame::Welcome` 加 `iceServers`（`#[serde(rename = "iceServers", default, deserialize_with = "deserialize_ice_servers")]`；`urls` 单串与数组都收，缺字段 / `null` / 畸形按空处理，数组里坏条目逐条丢）；`read_welcome` 与 `handle_server_frame` 的返回类型从 `RelayLimits` 变成 `RelayConfig { limits, ice_servers }`。**`iceServers` 只在会话建立时读一次**：中途换 STUN/TURN 会让两侧候选对不上，要换得等下一轮协商。空列表是隐私缺省——只有 host candidate，不填任何公共 STUN。
> 3. **`p2p.rs`（Windows only，约 630 行）**：`P2pLink`（input 侧）与 `P2pEvents`（事件流）分开，因为 `live` 的 `select!` 要一边 `&mut` 轮询事件、一边 `&` 发信令；内部是 `drive()` 驱动循环 + `Leg`（一轮协商）+ `Handler`（PeerConnection 回调）+ `pump()`（DataChannel 泵）。**glare 裁决：deviceId 字典序小的一方发起 offer**，两边算出的结论一致；`hello` 每次收到都重开一轮；没有 `hello` 时收到 `Offer` 也照接（能力门控只挡「我们主动发起」，不挡「对方已经发起了」）；candidate 在远端描述设好之前先缓冲；失败后只有发起方隔 5 秒重试；`Disconnected` 不算失败（只有 `Failed` / `Closed` 算）。**ICE 显式绑 `0.0.0.0:0`**——绑回环收不到真实 host candidate，显式绑是为了让 `PeerConnectionBuilder::<SocketAddr>` 的泛型可推断。对 webrtc 的 `Result` **一处 `unwrap` / `expect` 都没有**（release 是 `panic = "abort"`，一次 panic 会带走整个 App）。
> 4. **`link.rs` 是 cfg 中立门面**：Windows 上 `pub use super::p2p::{P2pEvent, P2pLink}`，其它平台给一份**同签名**的 stub（`next()` 用 `pending()`）。`release.yml` 仍然为 macOS / Linux 出包，那些目标不该因为 P2P 编不过。**注意谁来抓签名漂移**：`client-ci.yml` 的 rust job 只有 `windows-latest`，stub 那一支在 CI 上从不编译；真正能发现漂移的是本机在非 Windows 目标上的 `cargo check`，以及打 `v*` 标签时 `release.yml` 的 macOS / ubuntu 任务。
> 5. **心跳按 R28 拆成两条腿的探针（本次落地）**：中继腿的 ticker 分支**一行没改**（WS `Message::Ping`，中继自己回 Pong）；DC 腿在同一个 ticker 分支里发应用级 `pair.ping`，**独立标志** `dc_open` / `dc_awaiting_pong` / `dc_last_inbound`，超时**只置 `dc_open = false` 并把 `p2p` 置回 `Connecting`，绝不返回 `Outcome::Lost`**（否则 `abort_transfers` 会砍掉在途附件）。DC 入站只清 **DC 腿自己的**标志。DC 帧**不吃中继 pacer**（单一 pacer 会把 DC 上的可覆盖流压到 20 帧/秒，与 60Hz 冲突，R23）；**DC 侧**只有信令这条出站要走中继的 `pacer.acquire()`（中继帧与分片照旧各自过），且信令**不进 `state.reliable`**（那条队列上限 512，队满会挤掉最旧的聊天消息并把它退回 `pending`）。
> 6. **`build_frame()`**：心跳与信令这类低流量应用帧不塞进可靠队列，直接组帧发送。`handle_binary` 多一个 `Option<&link::P2pLink>` 参数，`pair.signal` 分支把载荷交给那条腿——**没有腿（非 Windows 目标）或载荷畸形就安静丢掉，也不产生回复**。
> 7. **`p2p` 状态与它的复位点**：`PairStatus` 加 `p2p: off | connecting | connected`（`P2pState`，`rename_all = "lowercase"`）；偏好页「连接状态」下面加**一行只读状态**，不写进 remote-cat / 主界面，也不新增事件常量。`PairStatus` 是长期存活的，所以**腿不在的每一段窗口都必须显式复位**，共 5 处：`start()`（连着的时候点「立即连接」/换中继，新会话还没起腿）、`disconnect()`、`fail_hard()`、`run_session` 的 `Reconnecting`（腿已随 `live` 返回被 Drop，而退避 30 秒 + 连接/welcome 超时 15+10 秒里不会再有 P2P 事件）、`live` 起腿之后（每次重连从 `Off` 开始）。漏掉 `Reconnecting` 那处是只读审计抓到的 P1：「重连中」+「已直连」会并存——单轮退避最长 36 秒（30 秒 × 1.2 的抖动）加上连接与 welcome 超时 15 + 10 秒，而中继一直不可达时这个假状态会一轮一轮地挂下去，直到 `disconnect()` / `fail_hard()` / 下一次 `live` 起腿才复位。`run_session` 结尾那处 publish 是**空操作**（能走到它的只有 `Command::Disconnect`，而它必然先加过 generation），所以没有在那里写复位。
> 8. **验证**：
>    - `cargo test --lib` = **107 passed / 7 ignored / 0 failed**（新增 `two_legs_negotiate_and_open_the_channel`：两条腿在同一进程里互喂信令，打通并双向收发；另有 `signal_payloads_round_trip_with_the_wire_shape` 与 `welcome_ice_servers_are_parsed_leniently`）。
>    - **真中继 e2e 6/6**（`127.0.0.1:8798`、心跳 2 秒）：新增 `two_clients_open_a_p2p_channel_through_the_relay`——两个 `PairManager` 连真实中继、换信令、开 DataChannel，**跨过 3 个心跳后两边仍是 `connected`**（DC 腿的探针超时窗口是两个心跳，所以这条断言等于证明 ping/pong 真的在 DC 上往返）；随后用**同 deviceId 的第三条连接**把 A 顶掉（中继给旧连接 4002 `REPLACED`，不是 fatal），断言那条 `state == "reconnecting"` 的广播里 `p2p` 已经复位成 `off`，最后断言手动断开后也复位成 `off`。原有 5 条继续全绿。
>    - 前端：`tsc --noEmit`、`pnpm test`（51 条）、`pnpm lint` 全过。
>    - **`Reconnecting` 那条断言做过红→绿**：临时删掉复位行重编，断言报 `left: Some("connected") / right: Some("off")`；恢复后绿。
> 9. **仍未做**：8c 的切换与回落（可覆盖流切到 DC、DC 掉线回落中继、附件分片走 `reliable` 通道）；**真机双端（两台机器、真实 NAT）的打洞验收仍是人工项**——本轮的 e2e 是同一台机器上的两个进程，只能证明「信令 → ICE → DataChannel → 探针往返」这条链路成立，证明不了跨 NAT 的可达率。

> **R30（Phase 8c 落地：可覆盖流切到 DC、DC 掉线回落中继）—— 提交 `cbc172f`**
>
> 1. **切换的门是两个标志**：`dc_open` 只说明 SCTP 协商完了，`dc_verified` 才是「这条腿真的过过数据」的证据（DC 入站置位）。计划里（R21 的「回落策略」段）早就写着「只有 DC `open` 且应用级 ping/pong 往返成功，才把可覆盖流切过去」，8b 只落了 `dc_open`，本次补上。只认 `dc_open` 的代价不是教条：默认心跳下探针超时是 **120 秒**，一条 open 但打不通的通道会让对端猫冻住两分钟，而两边 UI 都写着「已直连」。`dc_verified` 在三处复位（`ChannelOpen` / `ChannelClosed` / DC 探针超时）；`ChannelOpen` 时**立刻补发一枚 `pair.ping`**，否则要等一整个 tick（默认 60 秒）才验得到。不会死锁：DC 入站的回 pong 是无条件的，两边各自发 ping 就各自能验到。
> 2. **`p2p` 三态的语义跟着变精确**：`Connected` 只在「`dc_open` 且第一次 DC 入站」时发布（全仓库只有这一处 publish `Connected`），所以 **`p2p == connected` ⟺ 可覆盖流此刻在 DC 上**（`coverable_leg` 用的是同一对标志）。UI 那行「已直连」因此不会在通道还打不通时骗人；Phase 9a 的「60Hz 只在 P2P 生效」可以直接读这个字段。**发布也必须卡在 `dc_open` 上**（只读审计的 P2）：探针超时只把腿判为不可用、并没有关掉通道（超时 ≠ 关闭），超时之后对端恢复的流量照样会进来；不卡的话 UI 会在选路已经回到中继的情况下又亮起「已直连」，而且不会再自动复位。**已知限制**：探针超时之后这条腿不会自愈回 DC（超时后我们不再探它），要等通道真的关闭重开——这是保守方向的取舍（宁可一直走中继，也不把快照灌进一条已经不通的通道），9a 再决定要不要改。
> 3. **选路拆出 `flush_replaceable()`**：`flush()` 变回「只排可靠队列」——聊天、控制、附件、分片**永远走中继**（接收侧要求分片序号严格递增，而 `pet-state` 那条 DC 是 `ordered = false, max_retransmits = 0`）。可覆盖流走 `flush_replaceable(sink, leg, state, pacer)`：`leg = Some` 时 `leg.send(frame)`、**不吃中继 pacer**（R23）；`None` 时照旧 `pacer.acquire()` + 中继。写 `state.replaceable` 的只有两处（退避期与 `live` 的 `FlushReplaceable`），所以只有这两处需要它；会话开始那次 `flush_replaceable(None, …)` 是给退避期攒下的帧兜底（`state` 跨重连复用，确实会攒）。**顺序保持拆分前的样子**：先可靠、后可覆盖。
> 4. **`link.rs` 加 `CoverableLeg: Send + Sync`**（只有 `fn send(&self, frame: Vec<u8>)`），真 `P2pLink` 与 stub 各一份实现；选腿抽成 `coverable_leg(dc_open, dc_verified, link)`。抽 trait 只为一件事——**选路要能被单测直接验证**：真 `P2pLink` 需要一条真的 DataChannel，而「按 kind 与两个标志选腿」不该依赖它。
> 5. **DC 发送失败即丢帧**：`CoverableLeg::send` 返回 `()`，通道没开就丢，不回队也不补发——可覆盖流是绝对值快照，下一帧会盖掉它。**这不是 bug，别当 bug 修**。切换瞬间允许一帧乱序：接收侧只按 `envelope.id` 去重、没有 seq 高水位，而可覆盖流是绝对值语义，接受这个 blip。
> 6. **DC 侧没有任何 pacing**：R23 的「额度随传输层拆开」是 9a 的事，3Hz 下无所谓；9a 把上限提到 60Hz 时这里要一起看。
> 7. **顺手订正一处代码/文档不一致**：§4.2、R21「心跳归属」第二条、R28-3 一直把中继腿的标志叫 `relay_awaiting_pong` / `relay_last_inbound`，代码里却叫 `awaiting_pong` / `last_inbound`。8c 正是照着 §4.2 动这段代码的人，所以**把代码改成文档的名字**（纯重命名，零行为变化）。
> 8. **推迟 `reliable` 通道**：附件分片仍全程走中继。理由：`reliable` 通道要先定 ordered / 重传 / 背压、R22 的分片大小、以及「传输在途时延后切换」的状态机，是一次独立且风险更高的改动；§5.2 说「留给后续阶段」、§8 的 8c 说「该通道落地后再做，否则留在中继」。连带结论两条，都写进了 §10 的适用范围而不是假装满足：**「传输在途时延后切换」在 8c 不适用**（8c 没有任何传输被切），**「聊天不经服务器」仍然不成立**（要等 `reliable` 通道）。§8 因此补了一个承接阶段 **Phase 10：reliable 通道 + 附件分片走 P2P**——在此之前它在 §8 里没有任何阶段归属。
> 9. **验证**：
>    - 单测 **109 passed / 7 ignored / 0 failed**（新增两条）。`coverable_frames_take_the_data_channel_and_chat_never_does` 用假腿 + `RecordingSocket` 做**负向断言**：`leg = Some` 时可覆盖帧的字节**不出现**在中继那条线上、聊天只出现在中继上；`leg = None` 时可覆盖帧回到中继、腿上一片空白。`the_coverable_leg_needs_both_flags` 钉住选腿的门（两个标志都为真才给腿）。
>    - 真中继 e2e **6/6**（心跳 2 秒）：`two_clients_open_a_p2p_channel_through_the_relay` 在原来「跨 3 个心跳仍是 `connected`」之后补了一段——A 发 `pet-state`、B 收到 `EVENT_PET_STATE`，两边仍是 `connected`。**两层各自证明什么**：e2e 的 `p2p == connected` 现在等价于「两边都验过 DC」（第 2 条），所以它证明**门是开的**；单测的负向断言证明**门开着时帧走 DC、且不经过中继**。合起来才是「这一帧没经过服务器」。
>    - **诚实缺口**：真中继看不到这件事——它拿到的是 AEAD 密文、分不出帧 kind，只有「二进制帧个数」这一个粗粒度信号（同一条链路上还有走中继的 kind 8 信令），而且 WS Ping/Pong 压根不计桶。所以**不给中继加计数器**（那会为一次性测试去动它的对外契约，两份中继的契约要求一致）；「服务器转发量明显下降」留给 §10 的真机人工观察。
>    - 前端：`tsc --noEmit`、`pnpm test`（51 条）、`pnpm lint` 全过。`p2p` 那行的文案跟着改准：从「猫咪状态和聊天」改成「猫咪状态与输入统计」。
> 10. **仍未做**：`reliable` 通道与附件分片走 P2P（Phase 10）；Phase 9a 的 60Hz 与额度参数化；**真机双端（两台机器、真实 NAT）的打洞验收仍是人工项**——本轮 e2e 是同机两个进程，只能证明「信令 → ICE → DataChannel → 探针往返 → 可覆盖流切过去」这条链路成立，证明不了跨 NAT 的可达率，也证明不了「服务器转发量明显下降」。

> **R31（Phase 9a + 9b 落地：额度按传输层拆开、60Hz 上限、远端插值）—— 提交 `5f36bd8` / `8b49a50`**
>
> 1. **DC 那条腿有了自己的额度**：`DIRECT_FRAMES_PER_SECOND = 60` / `DIRECT_BURST = 60`，一个 `direct_pacer` 只服务 DC。中继腿的 `pacer` / `chunk_pacer` 与 `retune` **一行未改**——DC 上的帧不经过中继的计费点，拿中继额度去压它正好会把可覆盖流压回 20 帧/秒。9a 里 DC 上只有可覆盖流，所以 `direct_pacer` 目前只被 `flush_replaceable` 使用；附件分片与可靠帧上 DC 是 Phase 10。
> 2. **上限契约 `PairStatus.pet_state_hz`**：前端不再自己判断该用哪个上限（原来硬编码 `SNAPSHOT_INTERVAL_MS = 333`）。取值 = **当前生效传输的额度**：可覆盖通道可用（`dc_open && dc_verified`，与 `p2p == connected` 同一对标志）→ 60；否则中继推导帧额度 ≥ 60 → 60；否则 3。**这是一个台阶**：推导 59 → 3Hz、60 → 60Hz，看着突兀，但它是 §10 两条验收的直译——`min(60, max(3, 推导))` 会把 CF 从 3Hz 抬到 20Hz，违反「CF 版行为一字不变」。发布统一走 `publish_route`，`p2p` 与 `pet_state_hz` **一次写完**，不会出现「已直连但还按 3Hz 发」的中间态；`start` / `disconnect` / `fail_hard` / `Reconnecting` 都复位成 `Off` + 3Hz，welcome 重广告只重算 `pet_state_hz`。
> 3. **§6 的括号订正**：原文写「只把上限从 3Hz 提到 60Hz（**且只在 P2P 生效**）」，与 §10 的「自建中继广告 90 → 客户端能跑到 60Hz」互相矛盾。以 §10 为准：**上限跟着当前生效传输的额度走**。
> 4. **远端插值**：`remote-cat` 从 `setInterval(200)` 的慢更新换成常驻 `requestAnimationFrame`，指针位置按 `alpha = 1 - exp(-dt / tau)` 指数趋近最新快照。**tau 不是固定的 100ms**：固定的 100ms 会在 60Hz 下稳定引入 100ms 迟滞，而 §10 要的正是「明显更跟手」，所以取 `clamp(1.5 × 观测到的相邻快照间隔, 25ms, 150ms)`（观测值夹 `[16, 1000]`）：60Hz 下约 25ms，3Hz 下封顶 150ms。窗口被节流后的自愈靠第一帧的大 `dt`（`1 - exp(-dt / tau) ≈ 1` 直接吸附）——**这是设计，不是 bug**。
> 5. **只有真变了才写模型参数**：`appliedKey`（4 个布尔）与 `appliedX` / `appliedY`（阈值 0.001，远小于 0.02 的量化步长）。换模型后新模型是默认参数，所以 `live2d.load()` 成功后必须 `resetApplied()`，否则参数要等下一次变化才补上。
> 6. **验证**：
>    - 单测 **111 passed / 7 ignored / 0 failed**（新增 `the_direct_pacer_carries_the_sixty_hertz_budget`、`the_pet_state_ceiling_follows_the_effective_transport`）。`coverable_frames_take_the_data_channel_and_chat_never_does` 补两条断言：走 DC 的那一帧**不吃中继的 pacer**（中继额度只被那一条聊天消耗）、DC 那条腿的额度被扣掉一枚。
>    - 前端：`tsc --noEmit`、`eslint src`（0 problem）、`pnpm test`（51 条）全过。
>    - 独立只读审计：**无 P0 / 无 P1**；1 个 P2 是本轮多出的一处 rustfmt 差异，已改回基线（既有的 19 处差异不变，CI 也不跑 `cargo fmt --check`）。
>    - **没跑到**：真机双端目视「更跟手」（§10 的人工项）、`pnpm tauri build`。60Hz 是**上限**而不是目标（量化本身就是限流），所以「包量对比」也只在真机上有人工意义。
> 7. **仍未做**：Phase 10（`reliable` 通道 + 附件分片走 P2P，见 §8 的落地形状）、真机双端验收。

> **R32（Phase 10 落地：`reliable` 通道 + 附件分片走 P2P）—— 提交 `c7c9334`**
>
> 1. **分片大小成为「这一单的属性」**：`chunk_size` 从全局常量变成 `OutgoingTransfer` / `IncomingTransfer` 自己的字段（中继 512 KiB、DC 48 KiB），读块、seek、算「这一块该多少字节」全部用它；接收侧按 offer 的值 + 范围校验（`[4 KiB, min(512 KiB, 帧上限推导值)]`）**拒绝**而不是夹紧（§7），两侧的上界是同一个来源（`transfer::MIN_CHUNK_SIZE` 与 `transfer::max_chunk_size()`）。**校验失败从「本机报错」改成回一条 `transfer.reject`**：本地报错会让发送方停在 `AwaitingAccept` 等一个永远不来的回执（V1 没有停滞超时），而对未知 `transferId` 的 reject 在对端是 no-op，重复无害。
> 2. **能力门控**：`hello` 加可选 `features`（`reliable-channel`），`SIGNAL_VERSION` **保持 1**（硬相等判断，bump 会把新旧客户端之间的 `pet-state` P2P 一起关掉）；双向门控——offerer 只在对面声明过才建、answerer 只在对面声明过才认领，未知 label 一律不接管（8c 的 `on_data_channel` 是无条件认领，多给它一条通道会让它 last-wins 抢走 `pet-state` 的出站方向）。
> 3. **两条 lane 的探针与标志各自独立**：`reliable` 在自己的 `ChannelOpen` 时补一枚 `pair.ping`，由**它自己**的入站置 `reliable_verified`；入站产生的回执按原路返回（R30 的规矩）。UI 的 `p2p == connected` 仍然只表示**可覆盖流**在 DC 上，第二条腿不进 UI 状态。
> 4. **route 钉在传输会话上**（§8 形状 3，也就是「在途时延后切换」）：offer、分片、`transfer.complete` **以及回执**都走这一单的那条腿。收尾帧与 offer 走 `flush(..., leg, force = true)` **绕过背压判断**——背压恰好在最后一块之后翻假、完成帧绕了中继的话，接收侧会先看到「分片没收齐」把这一单判废（跨 lane 没有顺序保证）。回执按**这一单钉的 route** 选腿（`reply_leg` / `transfer_route`）：钉在**中继**上的一单绝不走 DC——DC 上丢一帧只有 `direct_lost()` 收尾，而它只管 `Route::Direct` 的会话，发送方会永远停在 `AwaitingAccept`（这一条是独立审计的 P1，见第 10 条）。
> 5. **失败收敛两处都接上**（§8 形状 4）：`ChannelClosed(Lane::Reliable)` 与探针超时都调 `direct_lost()`——按 §43 失败所有 `route == Direct` 的会话、在**中继**上显式发 `transfer.cancel`、并 `resend_pending_chat()`（库里 `pending` / `sent` 的聊天经中继重发，对端按 message id 去重并补 ack → 线上 at-least-once、UI / DB exactly-once）。只挂「通道关闭」不够：探针超时那一刻通道还是 open 的，只判不可用不关通道。
> 6. **DC 上的分片有自己的额度与背压**（§7 / §8 形状 6）：`DIRECT_CHUNKS_PER_SECOND = 160` / `DIRECT_CHUNK_BURST = 16`（= 15 × 512 KiB ÷ 48 KiB，与中继那条路同样的字节速率），**不与 60Hz 快照共用桶**、也不动中继的 `pacer` / `chunk_pacer`；发送缓冲 192 KiB（high = limit、low = 48 KiB；**`0` 等于无界，所以不传 0**），`writable` 是由 `OnBufferedAmountHigh/Low` 驱动的**同步**布尔标志——crate 的 `writable()` 是 async 的「等到有空间」，在 `live` 的 `select!` 分支里 await 它会把中继腿的入站读取一起挡住。背压只挡**注入**、不跳号：`mark_sent` 在真的交出帧之后才调用，被拒的那一块下次还是同一个 seq。
> 7. **背压标志在轮次边界复位**：`Leg::reset()` 里置回 `true`。新通道的 SCTP 发送缓冲从 0 开始只增不减，而 High / Low 都是跨阈值的**边沿**事件——不复位的话一轮重协商之后标志会永远停在 `false`：可靠帧只是绕回中继（无害），但钉在 DC 上的那一单再也发不出分片，而那条腿 open + verified、探针正常，`direct_lost()` 两个触发点都到不了。**残余**：上一轮的泵在通道关闭前后可能投一枚迟到的边沿事件（亚毫秒级，要两个任务正好交错），要彻底消掉得给 `pump` 传一枚 epoch，本轮不做。
> 8. **`chunk_wait` 的不变量**（§8 形状 7）：算 wait 与真正发帧用同一个 `next_sending_route()`；Direct 那一单在「背压翻假」或「腿不在」时给 `DIRECT_RETRY_INTERVAL = 5ms` 而不是 `ZERO`（`ZERO` 只留给「令牌够且背压允许」），否则 `select!` 会在 `Ok(false)` 与 `ZERO` 之间空转。
> 9. **没有拆提交**：`chunk_count` / `flush` / `handle_binary` / `send_next_chunk` 的签名改动是跨文件的，按「传输层 / 路由」拆会在中间留下不可编译的点，所以落成一个提交（§9 记一条）。
> 10. **验证**：
>
> - 单测 **120 passed / 7 ignored / 0 failed**（+9：新增 8 条 + 重写 1 条）。改名的 `coverable_frames_take_the_data_channel_and_chat_never_does` → `each_lane_keeps_its_own_stream_off_the_relay` 是**必须的**：Phase 10 之后聊天在可靠腿可用时也走 DC，「聊天永远只走中继」那条断言已经不成立；新断言是「两条腿都在时中继那条线上什么都没有」「背压翻假时聊天退回中继且照常扣中继额度」。
> - 真中继 e2e **6/6**（心跳 2 秒）。
> - `cargo check --lib` 0 warning；`cargo fmt --check` **37 处**（基线 46 处；逐行比对后本轮新增的代码没有一处落进去，反而顺手收掉了 9 处既有的）。
> - 独立只读审计（**新开的代理**，两轮）：**AUDIT: CLEAN**。第一轮 1 个 P1（回执绕 lane，见第 4 条）+ 2 个 P2（背压标志未在轮次边界复位、`MIN_CHUNK_SIZE` 只在测试里用导致 `cargo check` 报警），三条都已修并复审通过。
> - 前端本轮**没有改动**，所以没有再跑 `tsc` / `eslint` / `pnpm test`。
>
> 11. **仍未做**：**真机双端（两台机器、真实 NAT）验收**——包括「48 KiB 的分片真的过 DataChannel」这一条（假腿单测覆盖不到 SCTP 消息上限与 High/Low 在真实通道上的往返）、「服务器转发量明显下降」的人工观察；自建中继收摊（真机验收之前不收）。
>
>     **R33 订正**：上面括号里的两件事**不需要两台机器**，已改成单机自动化；真机项只剩跨 NAT 的打洞成功率（与视觉、打包）。

> **R33（单机验收：48 KiB 分片真的过 DataChannel + 服务器转发量的单机观测）—— 提交 `5f476f5`**
>
> 1. **起因**：R32 第 11 条把「48 KiB 的分片真的过 DataChannel」与「服务器转发量明显下降」一起归给了「两台机器、真实 NAT」的人工验收，而用户只有一台机器。这两件事要分开看：**跨 NAT 的打洞成功率**只能人工双端；而「这一帧过不过得了真的 SCTP / DataChannel」「走 DC 的那一单中继到底经手了多少字节」在本机就能自动化。
> 2. **`p2p` 层的三个单机用例**（`src-tauri/src/core/pair/p2p.rs`，不走中继、不需要第二台机器、跑在默认的 `cargo test --lib` 里）：同一进程里的两条 `P2pLink` 互喂信令，用的仍然是真的 host candidate、真的 ICE、真的 DataChannel；分片按 `TransferChunk` + 48 KiB 明文 + 真的 `PairCipher` 封帧，接收侧解密后再比对字节。
>    - `the_reliable_lane_carries_real_48kib_chunks`：24 块整块 + 尾巴上 1 个**1 字节的短块**（真实附件的最后一块总是短块）；断言两种上线长度（48 KiB + 帧头 + nonce + tag / 1 字节 + 帧头 + nonce + tag）、seq 严格递增、拼回来逐字节一致（补 R32 的「48 KiB 真的能过 SCTP」缺口，顺带把短帧也过一遍）。
>    - `the_send_buffer_really_fills_and_drains`：一口气灌 200 块（约 9.4 MiB，远多于 192 KiB 的发送缓冲上限）；断言 `writable` 真的翻假、9.4 MiB 全部逐字节到达、排空之后 `writable` 真的翻回真（补 R32 第 6 / 7 条只在假腿上量过的缺口：High / Low 事件在真通道上真的会来）。
>    - `dropping_the_leg_mid_burst_never_delivers_a_torn_chunk`：收到 2 块之后**真的拔掉发起方的腿**（`Drop` → 驱动循环收摊 → 关掉 PeerConnection）；断言窗口内到的每一块都是整块、有序、且是源字节的前缀——没有任何半块 / 错位块上线（V1 没有断点续传，一条被截断的帧会让接收侧把整单判废）。接收侧**多久**才发现对端没了由 ICE 的 consent freshness 决定（几十秒量级）、不由这一层决定，所以这里只做有界窗口的观察。
> 3. **文件走 DC 的 e2e**（`src-tauri/src/core/pair/e2e.rs`，`#[ignore]`，需要本机 relay）：`a_file_takes_the_data_channel_and_barely_touches_the_relay` 在真中继前面挡一个**纯 TCP 的字节计数器**（不解析 WebSocket，只数两个方向的字节），两个 `PairManager` 都连到它上面；等两边 `p2p` 都 `connected`、再等 2 秒（`reliable` 那条腿要自己 ping / pong 回来才算验过；UI 的 `p2p` 只描述可覆盖腿）后，发一个 1.5 MiB + 1 字节的附件。计数器的读法：传输窗口**前**已经 > 0（握手与 ICE 信令真的走过它，证明它在链路上），窗口内的增量就是「这条文件让服务器经手了多少字节」。**这条用例只在 `http://host:port` 形态的 relay 上真跑**：换成已经部署好的 https 中继它只会打印一行然后跳过（而 `--ignored` 全跑时跳过的用例仍算通过），所以那种情况下的「7/7」不代表这条量过。
> 4. **实测（本机自建中继 `127.0.0.1:8798`，心跳 2 秒）**：文件 **1,572,865 字节**（32 块整块 + 1 字节短块），传输窗口内中继经手 **0 字节（0%）**；传完两边 `p2p` 仍是 `connected`。
>
>    **反向对照（同一套计数方法）**：把同一个 1.5 MiB 负载交给 `two_clients_exchange_a_file_through_the_relay`（那条用例在 offer 之前**不等 P2P**，所以 `StartTransfer` 时 `reliable_verified` 还是假、这一单钉在中继上；用例自己不校验路由），中继经手 **1,572,864 字节的文件 + 约 5 KB 开销**（独立量到两次：1,577,789 与 1,578,024，差额是心跳落在窗口里的条数）。两侧合起来才能说明：计数器真的会看见走中继的那一单，而「0 字节」是真没走。
>
> 5. **仍然只能人工做的**：跨 NAT 的打洞成功率（UDP 封锁 / 跨境抖动下的真实成功率）、真机上「明显更跟手」的目视、`pnpm tauri build`。会话层「拔腿之后那一单失败、中继上补一条 cancel、中继那一单不受影响」的收尾现在有直接调用 `direct_lost()` 的单测（`losing_the_direct_leg_fails_its_transfer_and_cancels_it_over_the_relay`）——会话层没有真的拔腿入口，真拔腿只能在 `p2p.rs` 那一层做，两边合起来才是完整的那条路径。**残余**：`direct_lost()` 的两个触发点（`ChannelClosed(Reliable)` 与探针超时）本身仍没有用例驱动，那是 `live` 循环里的接线，本轮只钉住了动作本身。
> 6. **没有改产品代码**：本轮只加测试与文档。`SIGNAL_VERSION`、线上帧格式、pacer 参数、DC 的缓冲阈值一律未动。
> 7. **验证**：单测 **124 passed / 8 ignored / 0 failed**（+4）；真中继 e2e **7/7**（+1）；`cargo check --lib` 0 warning；`cargo fmt --check` 37 处（既有基线，本轮新增代码 0 处）。独立只读审计（**新开的代理**：两轮代码审计 + 一轮数字复核）**无 P0 / 无 P1**，两轮 P2 已修（对照句缺实测、短块没覆盖、注释精度、「7 条」在 CF 上会误导、本条的审计数字与对照字节数表述）。它自己跑到的数字：第一轮 **123 / 7 / 0 / 37**（那一轮还没有 `direct_lost()` 的用例），第二轮 **124 / 7 / 0 / 37**；两轮都复现了「窗口内中继 0 字节」，并独立量到反向对照 1,578,024 字节。
>
>    规划评审（只读代理）一轮 `CHANGES REQUIRED` 后 `REVIEW: AGREED`：它抓到 R33 初稿里那句「会话层收尾由假腿单测覆盖」是**假话**（`direct_lost()` 全仓只有生产调用点、一处测试都没有），要求补上面第 5 条那条单测——口径因此从「加强」变成「补上真缺口」。

---

# 1. 目标与非目标

## 1.1 目标

- 服务端不再绑死在 Cloudflare：提供自建中继，可与 CF 版**同一个客户端、同一套协议**互选。
- 自建中继上一个部署实例服务一对用户，服务器只做实时转发，不保存聊天、不保存文件。
- 传输层 P2P 优先：WebRTC DataChannel 打通后，状态 / 聊天 / 暂离 / 统计走 P2P；中继只留作信令、离线检测与回落。
- P2P 下桌宠状态上限提到 60Hz，远端平滑插值。
- 客户端配置项保持两个：`Relay URL` + `Pair Secret`。用户不需要理解 WebRTC 和 STUN/TURN（中继广告里没带 STUN/TURN 时，高级设置里可以手填）。

## 1.2 非目标（修订 v1 §88）

- 删除 v1 §88 里的 `WebRTC` 与 `NAT 穿透` 两条非目标（它们现在是本计划的主体）。
- 仍然不做：多人房间、注册、好友列表、账号体系、官方中转服务、断点续传、端到端设备迁移。
- 仍然不做 macOS / Linux 客户端（`server-relay` 的 Docker 镜像不算客户端）。
- 不用 CF 版跑 60Hz：CF 中继的额度固定 30 帧/秒，这是平台约束；60Hz 靠 P2P 或自建中继的更高额度。

---

# 2. 三种服务端形态

| 形态          | 部署方式                    | 谁维护 | 适用                                                            |
| ------------- | --------------------------- | ------ | --------------------------------------------------------------- |
| Cloudflare 版 | `wrangler` + Durable Object | 用户 A | 免费、零运维、不用域名；额度受限（30 帧/秒、每日请求数）        |
| 自建版        | `docker compose up -d`      | 用户 A | 完全自主；可放开到 60 帧/秒；可顺带跑 coturn                    |
| 裸机 / NAS    | 编译 `server-relay` 直接跑  | 用户 A | 有公网 IP 的机器；**必须有域名 + 受信任证书**，否则客户端连不上 |

自建版推荐组合：香港轻量云（腾讯云 / 阿里云）→ `1 核 1GB / 2 Mbps`。带宽参考（只算中继形态的转发）：

| 场景                  | 建议带宽    |
| --------------------- | ----------- |
| 猫咪状态 + 文字聊天   | 1 Mbps 就够 |
| 再加语音、偶尔图片    | 2~3 Mbps    |
| 图片/文件传输体验正常 | 5 Mbps      |
| 经常传几十/几百 MB    | 10 Mbps+    |

猫咪状态本身约几百字节 × 最高 60 次/秒，两人合计仍可忽略；吃带宽的只有文件。P2P 打通后连这些都不经过服务器。

---

# 3. server-relay 设计

## 3.1 目录

```text
server-relay/
├── Cargo.toml
├── src/
│   ├── main.rs        # 入口：读配置、监听（TLS 由 Caddy 终结）
│   ├── lib.rs         # 模块导出
│   ├── server.rs      # 配置（PAIR_* 环境变量）、HTTP 路由、鉴权
│   ├── protocol.rs    # 帧头 / 关闭码 / 控制帧 / 限流缺省值（与 protocol.ts 对齐）
│   ├── auth.rs        # HKDF-SHA256 → PAIR_AUTH_TOKEN、恒定时间比较、Bearer 解析
│   ├── http.rs        # 极小的 HTTP/1.1 读写（逐字节读请求头，避免多读掉 WS 帧）
│   ├── relay.rs       # 会话层：一对连接、令牌桶、转发、上下线通知
│   └── bin/generate-pair.rs   # 本地生成 Pair Secret（等价 generate-pair.mjs）
├── tests/relay.rs     # 真实 WebSocket 的端到端契约测试
├── Dockerfile / .dockerignore
├── docker-compose.yml # relay + caddy
├── Caddyfile          # 自动 TLS + WebSocket 反代
├── docker-compose.coturn.yml / turnserver.conf   # 可选：STUN/TURN
├── .env.example / .gitignore
└── README.md          # 部署文档（买机器 → 解析域名 → compose up → 填客户端地址）
```

## 3.2 必须逐条对齐的契约

以 `server-cloudflare/README.md` 的契约表为准，实现时对着它逐条核对，并用同一份客户端测试（`pair::e2e`）验证：

- `/health` 与 `/ws` 的路径、方法、返回结构；
- 三个请求头与各自的失败码（401 / 426 / 400 / 404 都是客户端的 fatal）；
- 只接受 binary；text 一律 `close 1008`；
- `server.welcome`（新增 `limits` / `iceServers` 可选字段，旧客户端忽略未知字段；`capabilities` 留到 Phase 8 再广告，见 R21）、`server.peer`；
- 令牌桶限流与三种关闭码；
- 收到 WebSocket Ping 必须回 Pong：CF 是平台自动回的，Rust 版靠 tungstenite 在读循环里自动回，`e2e.rs:674-746` 会断言这一点，别把读循环写成只处理业务帧；
- HKDF 派生与固定测试向量（R17）。

## 3.3 限流额度是「广告」而不是「常量」

见 R20 与 R25-3、R25-4。缺省广告值 = CF 值（30 / 20 / 12 MiB），自建时可通过环境变量放开。客户端 Pacer 按广告值推导，广告缺失时退回缺省；三个维度各自独立判定并有采信上限，客户端读到 `server.welcome` 之前不发送任何应用帧。

## 3.4 coturn（可选）

只在启用 P2P 且需要 TURN 兜底时装。要点：`lt-cred-mech` 静态凭据或 `use-auth-secret` 短期签名；UDP + TCP/443 双通道；单独配 TLS 证书；凭据只经已鉴权的 `server.welcome` 下发。

---

# 4. 客户端传输层抽象

## 4.1 `live` 泛型化（最小改动面）

出站侧**早就通用了**：`flush`、`enqueue_reply`、`send_next_chunk`、`send_frame` 都已经是 `S: Sink<Message> + Unpin, S::Error: Display`。真正绑死 WebSocket 的只有 `live` 的参数与那一行 `socket.split()`（`manager.rs:1206-1213`），调用点只有一处（`1116`）。

做法：把 `live` 的参数泛型成

```rust
T: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin,
E: Display
```

函数体只改 `transport.split()` 一行。四个辅助函数一个字都不用动，也不需要引入内部 enum。

**订正（Phase 8 规划评审）**：原文要求「再写一个把 WebSocket 当传输的适配器」，这是多余的——`WebSocketStream<MaybeTlsStream<TcpStream>>` 本来就同时实现了 `Sink<Message, Error = WsError>` 与 `Stream<Item = Result<Message, WsError>>`，两个 bound 是同一个 Error，中继这条路直接把 `PairSocket` 传进来即可，`manager.rs` 因此不再需要认识 `PairSocket` 这个类型。真正需要适配器的只有 DataChannel 那一侧（Phase 8b/8c），它按同一组约束接进来、直接收发 `tokio_tungstenite::Message`（所有应用数据都走 `Binary`）。

传输专有的帧处理点（以函数名定位；行号会漂，见 R21 开头的说明）：主动 `Close`（`Disconnect` 分支）、入站 `Message::Close` → `describe_close`、`Ok(_)` 兜住 Pong/Frame、ticker 的 `Message::Ping`（见 R21「心跳归属」）。DataChannel 适配器里 Ping/Pong 当空操作、`Close` → `dc.close()`、DC 关闭 → 回 `Ok(Message::Close(None))` 或结束流。

## 4.2 传输切换

按 R21：`live` 常读中继流，出站 sink 指向当前生效传输；信令钉在中继；切换按消息边界；DC 掉线立刻回中继。日志与错误文案改中性。

**入站分支（Phase 8 规划评审补的缺口）**：上面只说清了**出站**，入站同样是必须的。DC 上来的 `pet-state` 得有人读，所以 `live` 的 `select!` 要加一条 DC 入站分支，把字节喂给现成的 `handle_binary`（它只吃 `&[u8]` + `&mut SessionState`，天然与传输无关），并更新 **DC 腿自己的**探针标志（`dc_awaiting_pong`）。**不要**在这里更新中继腿的 `relay_awaiting_pong` / `relay_last_inbound`——那两个只由中继入站更新（见 R21「心跳归属」第二条与 R28）：让 DC 的 `pet-state` 去清中继腿的标志，等于把「中继静默半死」掩盖成一切正常。三条硬约束：

- **DC 断开绝不能变成 `live` 的返回值**：一旦返回 `Outcome::Lost`，`run_session` 就会 `abort_transfers` + 整条会话重连。DC 掉线只允许「把 active 切回中继」。
- DC 的入站读循环若放在单独任务里，帧要经一条 channel 送回 `live`，不能就地处理——`SessionState` 只有一个所有者。
- 「传输在途时延后切换」对可覆盖流天然成立（快照是绝对值、latest wins），但对将来走 `reliable` 的附件分片是硬要求（接收侧要求分片序号严格递增、V1 无断点续传）。

**落地（R30）**：8c 只切**可覆盖流**（`pet-state` / `stats`），门是 `dc_open && dc_verified`（也就是 R21「回落策略」段要求的「一次成功的往返」）；选路在 `flush_replaceable()` 里做，`link.rs` 的 `CoverableLeg` trait 让选路能被单测直接验证；DC 上的帧**不吃中继 pacer**。聊天、暂离、控制、附件分片全部留在中继。

---

# 5. P2P 设计

## 5.1 信令

- 新应用类型 `pair.signal`，走 `FrameKind::Ping`(8)，载荷含 `kind`（`offer` / `answer` / `candidate` / `hello`）等字段，AEAD 加密后经中继转发。
- 门控：只看**对端**——双方互发应用级 hello 声明 P2P 能力，收到对端声明才发起 ICE。旧中继不影响（`pair.signal` 走 kind 8，它本来就转发）；`server.welcome` 的 `capabilities` 只是可选增强。

## 5.2 两条 DataChannel

| 通道        | 选项                                  | 承载                                     |
| ----------- | ------------------------------------- | ---------------------------------------- |
| `pet-state` | `ordered = false, maxRetransmits = 0` | pet state、stats（可覆盖流，丢帧不重传） |
| `reliable`  | 有序可靠                              | 聊天、暂离、ACK、附件分片                |

Phase 8 只启用 `pet-state` 通道（只跑可覆盖流）；聊天等留在中继。`reliable` 通道留给后续阶段——附件分片要走它，不能走 `pet-state`（见 R21）。

## 5.3 生命周期

- 放 Rust（新增 `src-tauri/src/core/pair/p2p.rs`），不放 Vue：应用有 4 个 WebView（main / remote-cat / chat / preference），WebRTC 放前端会导致「谁负责 PeerConnection 生命周期」无解。
- 中继连接全程保留，同时是信令通道与离线检测。
- 重连时先切回中继，再重新协商。

## 5.4 STUN / TURN

见 R21：默认不填公共 STUN；设置页可覆盖；自建中继广告的地址优先。

---

# 6. 60Hz 与远端插值

- 发送侧：保持「变化立即发 + 尾随定时器」结构，上限按**当前生效传输的额度**取（R23 / R31）：可覆盖通道可用时 60Hz；中继广告额度够（推导 ≥ 60）时也是 60Hz；都不满足就留在 v1 的 3Hz。量化（0.02 / 0.2 / 布尔）继续当天然限流。
- Rust 侧：Pacer 额度随传输层参数化（R23），测试同步更新；上限本身通过 `PairStatus.pet_state_hz` 交给前端（R31），前端**不再自己判断**该用哪个上限。
- **DC 那条腿的额度就是 60Hz 上限本身**（`DIRECT_FRAMES_PER_SECOND`）：它既不吃中继的 `pacer` 也不吃 `chunk_pacer`，也不参与 `retune`（R23 / R31）。
- 接收侧：remote-cat 用常驻 `requestAnimationFrame` 按时间插值到最新快照（R31），tau 随观测到的快照间隔自适应，不再是固定的 `DECAY_INTERVAL_MS = 200` 轮询；TTL 释放逻辑（`TYPING_TTL_MS` / `CLICK_TTL_MS` / `SNAPSHOT_TTL_MS`）保留。
- 验收看的是「视觉上更跟手」，不是「包更密」。

---

# 7. 分片大小与 Pacer 参数化

见 R22 的清单。要点：

- `TransferOfferPayload.chunk_size` 早已存在，协议不动；
- 接收侧从「必须等于本机常量」改成**用 offer 的值 + 范围校验**（`[4 KiB, min(512 KiB, 帧上限推导值)]`），越界就**拒绝这条 offer**（R31 的订正：夹紧会让两端的分块长度算法不一致，每一块都会报「附件分片大小不对」，比早失败更难查）；「夹紧」落在**发送侧**选 `chunk_size` 时。两侧的范围必须是**同一对常量**（单一来源：`transfer::MIN_CHUNK_SIZE` 与 `transfer::max_chunk_size()`）。**已落地（R32）**：校验失败回一条 `transfer.reject`（不是只在本机 `emit_error`），否则发送方会停在 `AwaitingAccept` 等一个永远不来的回执。
- 中继 512 KiB / P2P 48 KiB（`P2P_CHUNK_SIZE`）；
- 分片 Pace 随传输层参数化（R23）：中继那条路仍是「通用额度 + 分片额度」两套；**DC 那条路有自己的分片额度**（`DIRECT_CHUNKS_PER_SECOND` / `DIRECT_CHUNK_BURST`），按「与中继那条路同样的字节速率」推导——15 × 512 KiB ÷ 48 KiB = 160，即 160 × 48 KiB/s ≈ 7.5 MiB/s，与今天中继那条路的天花板持平，不是新引入的激进值。它**不与 60Hz 快照共用桶**，否则 48 KiB 的块会把 60 枚/秒吃光、对端猫在整段传输里冻住。**已落地（R32）**：两条按 `TransferSession.route` 二选一，**绝不两套都扣**——DC 上的分片一枚中继令牌都不吃，中继那条路的帧也照旧不吃 DC 的桶。

---

# 8. 阶段划分

## Phase 7a：自建中继契约对齐

- 目标：`server-relay` 跑通 `pair::e2e` 的全部用例（同一份客户端代码、只换 URL）。
- 交付：`src/` + 单元测试 + 与 CF 版对齐的限流与关闭码；**以及客户端读取 `server.welcome` 的 `limits`**（缺失时回退 CF 缺省值，速率与突发按今天的四元组 20/20、15/10 与中继额度的比例放大，不 1:1）。`limits` 的消费必须跟广告一起进来，否则 Phase 9a 一旦被砍它就是死字段；「上限提到 60Hz」本身仍留在 9a。
- 验证：本地起 `server-relay`，跑 `cargo test --lib pair::e2e -- --ignored`；CF 版同一套用例继续通过（防回归）。

## Phase 7b：部署产物与文档

- 交付：`Dockerfile`、`docker-compose.yml`（relay + Caddy）、可选 coturn compose、`README.md`（香港节点、备案提示、带宽/内存建议、域名 + 证书要求、token 生成器）。可选项：给 `server-cloudflare` 的 welcome 补 `limits` / `capabilities` / `iceServers`（不补也能用，见 R21）。
- 验证：`docker compose up -d` 后 `/health` 通、`wss://` 端到端能连（真机跑客户端）。

## Phase 8a：传输层抽象重构（零行为变化）

- 交付：`live` 泛型化（`T: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin`，`E: Display`）。**没有适配器**——`PairSocket` 自己就满足这组约束（订正理由见 §4.1）。另加一条用例，把 `live` 跑在一条**不是 WebSocket** 的传输替身上，锁住「`manager.rs` 不再认识 WS 类型」这个不变量；
- 验证：改动前基线 `cargo test --lib` = 103 passed / 6 ignored（原文的「97 passed」是旧数，`f99a862` 那版在 `manager.rs` 加过用例）；改动后全绿（8a 落地时 104 passed / 6 ignored，含新增那条）；真中继 e2e 5 条全绿。这一步单独可审、单独可提交。

## Phase 8b：信令 + ICE + DataChannel

- 交付：`pair.signal` 应用类型、能力门控、`p2p.rs`（PeerConnection 生命周期）、`pet-state` 通道；**心跳拆成两条腿的探针**（中继腿保持 WS Ping 不变，DC 腿用应用级 `pair.ping`，两个独立标志——见 R28）；
- 验证：真机双端打通 P2P，能看到 DC open、ping/pong 往返；中继腿的行为不变（e2e 的 `stays_connected_across_heartbeats` 仍绿）。
- **已落地（`f3427cc`）**：见 R29。同一台机器上两个进程的真中继 e2e 已经覆盖「信令 → ICE → DataChannel → DC 腿探针往返」，以及两条腿的探针互不干扰；**真机双端（两台机器、真实 NAT）的打洞验收仍是人工项**。

## Phase 8c：切换与回落

- 交付：可覆盖流切到 DC 的逻辑、DC 掉线回落中继；**DC 腿探针超时只回落、绝不返回 `Outcome::Lost`**（R28）；附件分片要走 `reliable`（该通道落地后再做，否则留在中继）；
- 验证：关掉 P2P 通路后能自动回落且不丢聊天；**传输在途时延后切换**（等这一单结束再切，不在中途换传输）。
- **已落地（`cbc172f`）**：见 R30。切换门是 `dc_open && dc_verified`（R21「回落策略」段要求的「一次成功的往返」由 R30 补上）；可覆盖流走 `flush_replaceable`，DC 掉线或探针超时立刻回落中继。**附件分片仍留在中继**——`reliable` 通道推到 Phase 10，所以本条里「传输在途时延后切换」在 8c 不适用（没有任何传输被切），「不丢聊天」照旧成立（聊天全程在中继）。

## Phase 9a：额度参数化 + 60Hz 上限

- 交付：Pacer 随传输层参数化（DC 那条腿有自己的预算）、**`PairStatus.pet_state_hz` 这个新契约 + 前端消费**（上限只由 Rust 给出，前端不再硬编码 3Hz）；
- 验证：单测覆盖额度计算与上限推导（含「CF 缺省仍是 3Hz」「自建广告 90 → 60Hz」两个方向）；真机对比 3Hz 与 60Hz 的包量与视觉。
- **落地（R31）**：`DIRECT_FRAMES_PER_SECOND` / `DIRECT_BURST` = 60/60，`publish_route` 一次写完 `p2p` + `pet_state_hz`。上限的取值规则见 §6。

## Phase 9b：远端插值

- 交付：remote-cat 插值渲染（常驻 `requestAnimationFrame` + 自适应 tau）；
- 验证：真机目视「更跟手」，无抖动、无残影。
- **落地（R31）**：`tau = clamp(1.5 × 观测到的快照间隔, 25ms, 150ms)`；只有参数真的变了才写模型。

## Phase 10：reliable 通道 + 附件分片走 P2P

- 目标：让聊天、暂离、ACK 与附件分片也走 P2P（§5.2 的第二条 DataChannel，有序可靠）。§10 的「聊天不经服务器」要等它才成立。
- 交付：`reliable` 通道（有序、有重传）、上面那几类消息切过去、R22 的分片大小随传输层走、**传输在途时延后切换**（等这一单结束再切，不在中途换传输——接收侧要求分片序号严格递增、V1 无断点续传）；
- 验证：真机双端传一个附件，中途断掉 P2P 通路要能回落且不破坏这一单；对端是旧客户端时行为与 v1 一致。
- 说明：这一条在 R30 之前**在 §8 里没有归属阶段**（7a / 7b / 8a / 8b / 8c / 9a / 9b 都不管它），而 §10 的验收却依赖它。
- **落地形状（R32，规划评审 `AGREED`）**：
  1. **能力门控靠 `hello` 里的可选能力字段，`SIGNAL_VERSION` 保持 1**：那个版本是硬相等判断（不匹配就完全不协商），bump 会把 10↔8c 之间**连 `pet-state` 的 P2P** 一起关掉。两端都要门控——offerer 只在**对端声明过**时才建 `reliable`，answerer 只在**对端声明过**时才认领它；未知 label 一律不认领（8c 的 `on_data_channel` 是无条件认领任何通道，两条通道进来就是 last-wins、两个泵喂同一个 `Inbound`）。
  2. **两条腿各自的探针证据**：`pet-state` 用现有的应用级 `pair.ping`；`reliable` 在自己的 `ChannelOpen` 时也发一枚 `pair.ping`，它的入站置 `reliable_verified`。DC 入站的回复**按入站的那条通道原路返回**（R30 的约束）。`route == Direct` 可用 ⟺ `reliable_open && reliable_verified`。**不给 reliable 加 R30 那条「任何 DC 入站」的旁证**：那是无序通道上的廉价替代；有序通道上的真往返是更强的证据，而另一条 lane 的入站对这条 lane 没有证明力。
  3. **分片是唯一「不能有缺口」的流，所以按传输会话钉住 route**：`start_outgoing_transfer` 时选 route、把 `chunk_size` 写进 offer（Direct = 48 KiB，Relay = 512 KiB），`TransferSession` 存它，这一单的 `send_next_chunk` 全程走它。**这就是「传输在途时延后切换」**（不换腿，所以不会造成分片缺口）。聊天 / presence / 控制帧**不钉**，每帧按当前可用性选腿——两条腿两端都读，只有分片有「严格递增」约束。接收侧在收到 offer 时记下它的 lane（`handle_binary` 多收一个 lane 参数）。
  4. **失败收敛挂在「reliable 腿不可用」上**（探针**超时**与 `ChannelClosed(Lane::Reliable)` **两处**都调同一个 `direct_lost()`）：按 §43 失败所有 `route == Direct` 的会话（两个方向都算），并且**在中继上显式发一条 `transfer.cancel`**——不能假定两端在同一时刻拿到同一个事件，半死的腿正是「一端以为还在传、另一端什么都没收到」的形状；对端收到未知 `transferId` 的 cancel 是 no-op，重复无害。
  5. **「聊天不丢」靠 DB 兜底而不是在途缓冲**：`reliable` 腿掉时调 `resend_pending_chat()`（库里 `pending` / `sent` 的聊天经中继重发，对端按 message id 去重并补 ack）。不变量：**线上 at-least-once、UI / DB 按 message id exactly-once**——正是它让「丢掉在途帧」变安全。
  6. **DC 上的分片有自己的额度与背压**：额度见 §7（与 60Hz 快照**不共用桶**）；另外配 `with_data_channel_send_buffer_limit`（**`0` 等于无界，别传 0**）并用 `OnBufferedAmountHigh` / `OnBufferedAmountLow`（配 `set_buffered_amount_high_threshold` / `set_buffered_amount_low_threshold`）维护一个 writable 标志。阈值按「几个分片」定（约 limit 192 KiB / high = limit / low ≈ 48 KiB），**不照抄 crate 文档的 16 MiB**——那段建议是给只跑批量数据的通道写的，而我们的 `reliable` 是三用的（聊天 / 控制 / 分片），排队量直接等于聊天的队头延迟。`writable()` 是 **async 的「等到有空间」**，不能在 `live` 的 `select!` 分支里 `await`（会把中继腿的入站一起挡住，正是 R18 注释防的那件事）；非阻塞那一半用 `try_send` 的语义。
  7. **`chunk_wait` 的不变量**：算 wait 与真正发帧必须用**同一个** `state.next_sending_transfer()`；并且只要这一轮因为取令牌 / 背压 / 会话状态而**没发出去**，wait 就**不能**是 `ZERO`，要给一个 5ms 量级的轮询间隔——否则 `Ok(false)` + `ZERO` 就是忙循环。
  8. **背压拒绝绝不能造成跳号**：有序可靠通道上唯一的「丢」只能来自我们自己的拒绝，而接收侧要求 seq 严格递增。源头（writable 标志）负责挡住注入；万一 `try_send` 仍然满载，按「这一单失败 + cancel」处理，**不跳过、也不重发同一 seq**。
  9. **两条已知取舍**：`Input::Failed` 来自**任一** lane 的泵退出，所以一条通道关闭也会触发整条腿重协商（可接受：关掉的通道本来就要重开一轮）；未知 label 的通道在 webrtc 下仍会缓冲对端发来的数据（风险表已记）。Phase 10 之后 `p2p == connected` 仍然只表示**可覆盖流**在 DC 上，第二条腿不暴露成 UI 状态。

- **落地（R32）—— 提交 `c7c9334`**：上面 9 条的形状逐条落地。落地的过程中又补了三处形状里没写到、但同源的东西，都写进 R32 了：
  1. **收尾帧与回执也必须跟着这一单的 route**：`transfer.complete` 与 offer 走 `flush(..., leg, force)` 的**强制**路径（`force` 绕过背压判断）——背压恰好在最后一块之后翻假、完成帧却绕了中继的话，接收侧会先看到「分片没收齐」把这一单判废（跨 lane 没有顺序保证）。accept / reject / cancel 按**这一单钉的 route** 选腿（`reply_leg` + `transfer_route`），钉在中继上的一单绝不走 DC：DC 上丢一帧只有 `direct_lost()` 收尾，而它只管 `Route::Direct` 的会话，发送方会永远停在 `AwaitingAccept`。
  2. **越界的 offer 回 reject，而不是本机报错**（§7 那条的落地形状）：参数校验失败以前只在本机 `emit_error`，发送方会停在 `AwaitingAccept` 等一个永远不来的回执（V1 没有停滞超时）；现在回一条 `transfer.reject`，两端立刻按 §43 收尾。
  3. **背压标志在轮次边界复位**（`Leg::reset()`）：新通道的 SCTP 发送缓冲从 0 开始只增不减，而 High / Low 都是跨阈值的**边沿**事件，不复位的话一轮重协商之后标志会永远停在 `false`，钉在 DC 上的那一单再也发不出分片。
- **验证（R32）**：单测 **120 passed / 7 ignored / 0 failed**（新增 8 条、重写 1 条：`coverable_frames_take_the_data_channel_and_chat_never_does` 改成 `each_lane_keeps_its_own_stream_off_the_relay`，因为 Phase 10 之后聊天也走 DC）；真中继 e2e **6/6**；`cargo check --lib` 0 warning；`cargo fmt --check` 37 处（基线 46 处，新增代码无差异）。独立只读审计（新代理，两轮）：**AUDIT: CLEAN**（第一轮的 1 个 P1 + 2 个 P2 已全部修掉并复审通过）。
  - **诚实缺口（R32 当时）**：48 KiB 的分片真的过 DataChannel 这条路径只有**假腿**单测覆盖；真中继 e2e 只发了小的 pet-state / ping。也就是「一块 48 KiB 的 SCTP 消息 + `writable` 背压 + High/Low 事件真的能翻回来」在真实通道上还没有自动化用例跑过。
  - **R33 补上（单机，不需要第二台机器）**：上面这条缺口已经用三个本机用例盖掉——24 块 48 KiB + 1 个 1 字节短块真的过 DataChannel 并逐字节比对、192 KiB 发送缓冲真的会压满并在排空后翻回 `writable`（200 块 ≈ 9.4 MiB）、拔腿时不会有半块上线；另外加了「文件走 DC，中继在那个窗口里经手 0 字节」的 e2e（中继前挂一个纯 TCP 字节计数器，反向对照约 1.58 MB：两轮分别量到 1,577,789 / 1,578,024）。会话层那半（`direct_lost()` 收尾 + 中继上补 cancel）由 `manager.rs` 的单测直接钉住。跨 NAT 的成功率仍是人工项。

---

# 9. 提交拆分建议

```text
refactor(pair): abstract the pair transport behind a generic sink
feat(pair-server): add a self-hosted relay
docs(pair-server): document self-hosted relay deployment
feat(pair): add webrtc signaling over the relay
feat(pair): add the webrtc p2p transport with relay fallback
feat(pair): pace outbound frames by the advertised transport limits
perf(pair): raise the pet state ceiling and interpolate remotely
```

实际落地时，8b 把上面第 4、5 两条合成了一个提交（`f3427cc`，`feat(pair): add webrtc signaling and the p2p link`）：信令与 P2P 传输同属一次交付。第 5 条标题里的「relay fallback」指的是切换与回落，那半属于 8c，届时单独落。

Phase 9a 与 9b 也拆成了两个提交（R31）：`5f36bd8`（`feat(pair): give the data channel its own pacing budget and a 60hz ceiling`）与 `8b49a50`（`feat(pair): interpolate the remote cat between snapshots`）。上限契约与渲染插值是两件独立的事，放在一起反而看不清各自的验证面。

单机验收（R33）同样拆成两条：`test(pair): ...`（三个 `p2p` 用例 + 一条 `direct_lost()` 的 `manager` 用例 + 一条挂字节计数器的 e2e）与 `docs(pair): record the phase 10 single-machine acceptance`（本条记录与 §8 / §10 / §11 / §12 的订正）。测试与文档分开的理由是两者跑的地方不同：前者进 CI（`cargo test --lib`），后者的价值只在读文档时体现。

Phase 10 落成**一个**提交（R32）：`c7c9334`（`feat(pair): carry the reliable streams over a second data channel`）。`chunk_count` / `flush` / `handle_binary` / `send_next_chunk` 的签名改动是跨文件的，按「传输层 / 路由」拆会在中间留下不可编译的点，所以没有拆——一条能编译的提交比两条好看但不能编译的强。

---

# 10. 验收标准

**自建中继**

- 同一份客户端只改 URL 就能连自建中继，功能与 CF 版逐条一致（状态、聊天、附件、语音）。
- `/health` 通；错误 token 被 401 拒；第三个连接被 `PAIR_FULL 4003` 拒；旧连接 120 秒后被顶替。
- 客户端读 `server.welcome` 的 `limits` 并按缺省回退；把自建中继的额度调高后，客户端放行速率按今天的余量比例随之提高（不 1:1）。

**P2P**

- 打洞成功时，猫咪状态与聊天不经服务器；服务器转发量可观察到明显下降。**（R33：单机可量化——中继前面挂一个纯 TCP 字节计数器，走 DC 的 1.5 MiB 附件实测窗口内中继经手 0 字节。）**
- 打洞失败时自动走中继，用户无感知。
- 对面是**旧客户端**时不发起 ICE（旧中继不影响，`pair.signal` 走 kind 8 本来就转发），行为与 v1 一致。
- P2P 掉线后能自动回落中继，聊天不丢、附件按 §43 收尾。

**适用范围（R30 补，R32 再补，R33 定终）**：8c 之后，上面第一条只有**猫咪状态与输入统计**成立。**Phase 10 落地（R32）之后，「聊天不经服务器」与「附件不经服务器」也成立**——条件是 `reliable` 那条腿 open + verified；对端是旧客户端、打洞失败、探针超时、或背压挡住的那些时刻自动回落中继（用户无感知，聊天靠 DB 补发、附件按 §43 收尾）。第三条（旧客户端行为与 v1 一致）与第四条（掉线回落、聊天不丢）不受影响：前者靠 `hello` 的可选能力字段，后者今天本来就是这样。

**「48 KiB 分片真的过 DataChannel」与「服务器转发量下降」已经是单机自动化证据（R33）**，不再需要两台机器：三个 `p2p` 层用例（跑在默认的 `cargo test --lib` 里，含 1 字节短块的尾帧）+ 一条挂字节计数器的中继 e2e（`--ignored`，只在 `http://host:port` 的自建中继上真跑；反向对照用同一套方法量到约 1.58 MB 过服务器：两轮分别 1,577,789 / 1,578,024）+ 一条直接调 `direct_lost()` 的单测（会话层收尾）。**真机双端只剩跨 NAT 的打洞成功率**，以及「更跟手」的目视与 `pnpm tauri build`。

**60Hz**

- P2P 下桌宠状态上限 60Hz、空闲时 0 发送。
- 自建中继把额度调高（例如 90 帧/秒，客户端按 2/3 推导得 60）后，客户端能跑到 60Hz 上限而不被中继限流；CF 版行为一字不变。
- 远端猫在 60Hz 下明显更跟手（插值生效），无抖动残影。

**回归**

- v1 的验收标准（§87）全部继续满足。

---

# 11. 测试与验证

| 层       | 手段                                                                                                                                                                                                                            |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 静态     | `cargo check --lib`、`cargo fmt --check`（注意仓库既有差异）、`eslint src`、`tsc --noEmit`                                                                                                                                      |
| 单元     | `cargo test --lib`（含新增的额度、夹紧、信令编码用例）、`vitest run`（前端 mapper 与插值纯函数）                                                                                                                                |
| 中继 e2e | 对 CF 与自建两份中继各跑一次 `cargo test --lib pair::e2e -- --ignored`（7 条，含「走 DC 的附件让中继经手多少字节」那条：中继前挂纯 TCP 字节计数器。**那条只认 `http://host:port`**，CF 那种 https 部署上它会跳过、仍记作 pass） |
| 单机 P2P | `cargo test --lib pair::p2p`（真 DataChannel 上的 48 KiB 分片 + 1 字节短块、192 KiB 背压压满 / 排空、拔腿时不会有半块；不需要中继与第二台机器）                                                                                 |
| 真机     | 双端跑 `pnpm tauri dev`（或安装包），验证跨 NAT 的打洞成功率、60Hz 视觉                                                                                                                                                         |
| 打包     | `pnpm tauri build --debug`；确认体积与编译时间变化                                                                                                                                                                              |

新增的 CI job 已在 `.github/workflows/client-ci.yml` 落地（R24-2、R27），单元层与静态层的 `tsc` 现在会在 PR 上自动跑（`cargo fmt --check` 与 `eslint src` 仍不在门禁内）。触发用 `push` + `pull_request` + `workflow_dispatch`，`paths` 过滤 `src/**`、`src-tauri/**`、`scripts/**`、`pnpm-workspace.yaml`、`Cargo.toml`、`Cargo.lock`、`package.json`、`pnpm-lock.yaml`、`tsconfig*.json`、`vitest.config.ts` 和 workflow 自身。**不**过滤 `vite.config.ts` / `uno.config.ts`——这两个 job 都不读它们（`vitest run` 只读 `vitest.config.ts`）。`scripts/**` 与 `pnpm-workspace.yaml` 必须留在列表里：前者是 `pnpm build:icon` 直接执行的东西，后者决定 `pnpm install` 能否成功。

**实际形状（与规划评审的建议有两处必改，见 R27）**：

- `jobs.rust` 走 **`windows-latest`**：`checkout@v4` → `pnpm/action-setup@v4`（`version: 11`）→ `setup-node@v4`（`node-version: 24`、`cache: pnpm`）→ `pnpm install --frozen-lockfile` → **`pnpm build:icon`** → `dtolnay/rust-toolchain@stable` → `swatinem/rust-cache@v2`（`workspaces: .`，`Cargo.lock` 在仓库根）→ `cargo test --lib`。放 Windows 有三条理由：`release.yml` 的 ubuntu 任务必须装 `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libudev-dev patchelf xdg-utils pkg-config libasound2-dev` 才编得过 `cpal` / `rdev` / `gilrs`，Windows 任务一个系统依赖都不装；`#[cfg(windows)]` 的 `p2p` 只在 Windows 编译，放 Linux 等于 8b/8c 的关键代码没有 CI 兜底；真机验证也在 Windows。
- **图标生成是 rust job 的必需前置**：`tauri-build` 在 Windows 上要读 `icons/icon.ico` 生成 Windows 资源文件，而 `src-tauri/icons` **不入库**（`src-tauri/.gitignore` 里就写着 `icons`）。`release.yml` 是靠 `pnpm tauri build` 的 `beforeBuildCommand`（= `pnpm build`，内含 `build:icon`）顺带生成的，这个 job 必须显式补同一步，否则干净检出上 `cargo test` 直接报 `icons/icon.ico not found`。图标由入库的 `src-tauri/assets/logo.png` 生成。
- `jobs.web` 走 `ubuntu-latest`：`pnpm/action-setup@v4`（`version: 11`）+ `setup-node@v4`（`node-version: 24`、`cache: pnpm`）→ `pnpm install --frozen-lockfile` → `pnpm test`（`vitest run`，`environment: 'node'`，不需要浏览器依赖）→ `node node_modules/typescript/bin/tsc --noEmit`。
- 两个 job 都加 `permissions: contents: read`。
- **Node 必须是 24（≥22.13），不能用 20**：pnpm 11 用了 `node:sqlite`，在 Node 20 上连 `pnpm -v` 都崩。
- **`pnpm-workspace.yaml` 的 `allowBuilds` 是必需的**：pnpm 11 默认拒绝执行依赖的 build script，只要有一条被忽略就让 `pnpm install` 以 `ERR_PNPM_IGNORED_BUILDS` 退出 1；更麻烦的是 `pnpm run` 前的那次依赖检查会**再跑一次 install**（`verify-deps-before-run` 默认值 `install`），命令行上的 `--config.strict-dep-builds=false` 传不进那一次，于是 `pnpm test` / `pnpm build:icon` 会跟着一起红。仓库根因此新增 `pnpm-workspace.yaml`，显式放开 `@parcel/watcher`、`esbuild`、`simple-git-hooks` 三个（都不影响构建产物）。副作用：本机 `simple-git-hooks` 会被真装上（AGENTS.md 已如实记录）。
- CI 上**不要**加 `--store-dir .pnpm-store`（那是本机 store 的特例，见 AGENTS.md），也**不要**加 `cargo fmt --check`（仓库既有 diff 会让它一直红）。
- 成本：Windows runner 上 `cargo test --lib` 要冷编译整个 tauri lib，务必配 rust-cache，不要做 matrix。
- **覆盖边界**（P2，别当成全量门禁）：`cargo test --lib` 只覆盖 lib target，不含 bin target 与 `--all-targets`；`tsc --noEmit` 只覆盖 `.ts`（仓库没装 `vue-tsc`，`.vue` 里的类型问题仍靠 review 与实跑）。`vite build` 与 `eslint` 不在这个门禁内。

**R24-6 的测量结果（依赖落地那一刻量的，见 R27-3）**：`cargo build --release` 总墙钟 **5 分 23 秒**（323.4 s），`target/release/bongo-cat.exe` = **12,761,088 字节（约 12.2 MiB）**，`Cargo.lock` 净新增 **41 个 crate 名**（lock 条目 778 → 841，差的 22 个是已有 crate 多出的第二个 semver 条目，如 `asn1-rs` 0.6/0.7、`x509-parser` 0.16/0.18、`der-parser` 9/10）。仓库**没有 HEAD 基线**（至今 0 个 tag、没跑过 `release.yml`），所以这组数字是「有依赖的绝对值」而不是增量；要不要进一步按 feature 门控，等 8b/8c 真写完之后再看。

---

# 12. 风险与未决

| 风险                                                  | 影响                                                                                                     | 缓解                                                                                                                                                                                                                                             |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `webrtc-rs` 在 3 个 Windows release 目标上编译失败    | Phase 8 无法交付                                                                                         | 编译 spike 已过（R24-1）；4 个非 Windows 目标已用 `cfg` 门控排除                                                                                                                                                                                 |
| Windows 定时器精度 15.6ms 影响 60Hz                   | 发不出真正 60Hz                                                                                          | 事件驱动而非固定 16ms 定时器                                                                                                                                                                                                                     |
| SCTP 消息上限（实测默认 256 KiB，不是 64 KiB）        | 附件分片在 P2P 上失败                                                                                    | 48 KiB 保守默认 + 单机真 DataChannel 用例（**R33**：24 块 48 KiB + 1 字节短块逐字节比对、200 块 ≈ 9.4 MiB 压满再排空）                                                                                                                           |
| 可靠腿掉了之后在传的附件没人收尾                      | 会话永久留在表里：发送方停在 `AwaitingAccept` / 接收方等分片，V1 没有停滞超时                            | **已落地（R32 / R33）**：`direct_lost()` 本地判失败 + 中继上补 `transfer.cancel`；`manager.rs` 的单测直接调用它钉住「Direct 那单失败、中继那单不受影响、cancel 只走中继腿」                                                                      |
| 「服务器转发量下降」没有可复现的观测手段              | 这条验收只能靠目视，改坏了也看不出来                                                                     | **已落地（R33）**：单机在中继前挂一个纯 TCP 字节计数器，走 DC 的 1.5 MiB 附件实测窗口内中继经手 0 字节（`a_file_takes_the_data_channel_and_barely_touches_the_relay`）                                                                           |
| `panic = "abort"` 下 webrtc 内部 panic 会带走整个 App | 一条 P2P 连接的问题升级成整个应用崩溃                                                                    | `p2p.rs` 里对 webrtc 的 `Result` 一律不许 `unwrap` / `expect`，DC / ICE 失败只走回落                                                                                                                                                             |
| 自签 / 裸 IP 无法连自建中继                           | 部署文档承诺的「compose up 就能用」落空                                                                  | 文档强制域名 + Caddy；可选自签开关                                                                                                                                                                                                               |
| UDP 被封 / 跨境抖动                                   | P2P 打洞失败率高                                                                                         | TURN 备 TCP/443；打不通就走中继                                                                                                                                                                                                                  |
| STUN 暴露公网 IP 与 README 隐私承诺                   | 隐私承诺被质疑                                                                                           | 默认不填公共 STUN，设置页写明                                                                                                                                                                                                                    |
| `webrtc` 拉长编译与体积                               | 发布耗时、安装包变大                                                                                     | 依赖落地那一刻量一次（R24-6），必要时按 feature 门控                                                                                                                                                                                             |
| 两条腿的心跳探针共用一个标志                          | 中继静默半死被 DC 流量掩盖：DC 上 pet-state 照常流动，聊天 / 信令 / 离线检测全哑却看起来正常，也不重连   | 两条独立标志——中继腿只由中继入站清除；DC 腿超时只回落（R21「心跳归属」第二条、R28 第 3 条）                                                                                                                                                      |
| DC 已 open 但打不通（ICE connected 之后半死）         | 可覆盖流灌进黑洞：对端猫冻住，而两边 UI 都显示「已直连」                                                 | 切换门要 `dc_open && dc_verified`（DC 入站才算验过），`ChannelOpen` 立刻补一枚 ping，`p2p = connected` 与选路用同一对标志；**验过之后又静默半死**的那一半靠 R28 的 DC 探针超时兜底（窗口最长约 2 个心跳，默认 120 秒），这期间选路仍在 DC（R30） |
| DC 的发送缓冲不设上限就不阻塞（R31 记）               | 慢链路下注入量只受令牌桶约束，缓冲无界增长                                                               | **已落地（R32）**：`with_data_channel_send_buffer_limit` 192 KiB（high = limit、low = 48 KiB）+ 由 High/Low 事件驱动的同步 `writable` 标志，源头挡住注入；背压翻假时聊天退回中继、分片那一单等 5ms 再试（`force` 只用在收尾帧与 offer 上）       |
| 回执走错 lane 会让发送方无超时死等（R32 的 P1）       | 钉在中继上的一单，它的 accept 走 DC 一旦丢帧就没人收尾（`direct_lost` 只管 Direct 的会话），两端永久挂起 | 回执按**这一单钉的 route** 选腿（`reply_leg` / `transfer_route`），中继那一单绝不给 DC 腿；Direct 那一单给腿但被背压挡回中继也不影响正确性（那一侧本来就有 `direct_lost`）                                                                       |
| DC 那条腿的背压标志跨轮次残留（R32）                  | 一轮重协商之后标志永远为假：钉在 DC 上的那一单再也发不出分片，而探针/可用性一切正常                      | `Leg::reset()` 里置回 `true`（轮次边界）；残余「上一轮的泵投来一枚迟到边沿」要彻底消掉需给 `pump` 传 epoch，本轮不做（R32-7）                                                                                                                    |
| 未知 label 的 DataChannel 仍会被 webrtc 缓冲          | 恶意对端可以多开通道占内存                                                                               | 只认领声明过的两条；不认领的通道不 poll（对端发来的数据仍会被缓冲，所以记在这里）                                                                                                                                                                |
| 任一条 DC 通道的泵退出会重开整条腿                    | 一次重协商（十几秒），期间可靠流全部回中继                                                               | 可接受：两条通道在同一条 SCTP 关联上，关掉的那条本来就要重开一轮                                                                                                                                                                                 |

未决：自建中继是否默认广告 60 帧/秒（还是留给环境变量）。 另一个未决：探针超时之后那条腿要不要自愈回 DC（今天不自愈，要等通道真的关闭重开；保守方向的取舍，9a 再定，见 R30）。

**已定**：`webrtc-rs` 是否暴露 SCTP `max-message-size`——0.21 默认 256 KiB（`MAX_MESSAGE_SIZE`）且超限之前由实现自己分片，48 KiB 不必改（理由见 R22）。

---

# 13. Codex 执行要求

- 每次提交前用独立只读 subagent 审计，PASS 才能提交；新功能新开审计代理，复审复用同一代理。
- 规划改动先与 subagent 讨论达成一致（沿用 v1 的流程）。
- 主代理改代码，subagent 只做只读分析与审计。
- 只承诺 Windows 客户端的真机验证。
- 报告里区分静态检查、构建与真机运行三种证据。

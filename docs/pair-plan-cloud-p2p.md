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

- 发送侧：保持「变化立即发 + 尾随定时器」结构，只把上限从 3Hz 提到 60Hz（且只在 P2P 生效）。量化（0.02 / 0.2 / 布尔）继续当天然限流。
- Rust 侧：Pacer 额度随传输层参数化（R23），测试同步更新。
- 接收侧：remote-cat 把 `DECAY_INTERVAL_MS = 200` 的慢更新改成随快照驱动的插值渲染（在 60 FPS 下按时间插值到最新快照），TTL 释放逻辑（`TYPING_TTL_MS` / `CLICK_TTL_MS` / `SNAPSHOT_TTL_MS`）保留。
- 验收看的是「视觉上更跟手」，不是「包更密」。

---

# 7. 分片大小与 Pacer 参数化

见 R22 的清单。要点：

- `TransferOfferPayload.chunk_size` 早已存在，协议不动；
- 接收侧从「必须等于本机常量」改成「用 offer 的值 + 夹紧（≥4 KiB，≤512 KiB 且 ≤ 帧上限推导值）」；
- 中继 512 KiB / P2P 48 KiB；
- 分片 Pace 改成按字节或随传输层参数化，避免 48 KiB × 15 的低速。

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

## Phase 9a：额度参数化 + 60Hz 上限

- 交付：Pacer 随传输层参数化、60Hz 上限；
- 验证：单测覆盖额度计算；真机对比 3Hz 与 60Hz 的包量。

## Phase 9b：远端插值

- 交付：remote-cat 插值渲染；
- 验证：真机目视「更跟手」，无抖动、无残影。

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

---

# 10. 验收标准

**自建中继**

- 同一份客户端只改 URL 就能连自建中继，功能与 CF 版逐条一致（状态、聊天、附件、语音）。
- `/health` 通；错误 token 被 401 拒；第三个连接被 `PAIR_FULL 4003` 拒；旧连接 120 秒后被顶替。
- 客户端读 `server.welcome` 的 `limits` 并按缺省回退；把自建中继的额度调高后，客户端放行速率按今天的余量比例随之提高（不 1:1）。

**P2P**

- 打洞成功时，猫咪状态与聊天不经服务器；服务器转发量可观察到明显下降。
- 打洞失败时自动走中继，用户无感知。
- 对面是**旧客户端**时不发起 ICE（旧中继不影响，`pair.signal` 走 kind 8 本来就转发），行为与 v1 一致。
- P2P 掉线后能自动回落中继，聊天不丢、附件按 §43 收尾。

**60Hz**

- P2P 下桌宠状态上限 60Hz、空闲时 0 发送。
- 自建中继把额度调高（例如 90 帧/秒，客户端按 2/3 推导得 60）后，客户端能跑到 60Hz 上限而不被中继限流；CF 版行为一字不变。
- 远端猫在 60Hz 下明显更跟手（插值生效），无抖动残影。

**回归**

- v1 的验收标准（§87）全部继续满足。

---

# 11. 测试与验证

| 层       | 手段                                                                                             |
| -------- | ------------------------------------------------------------------------------------------------ |
| 静态     | `cargo check --lib`、`cargo fmt --check`（注意仓库既有差异）、`eslint src`、`tsc --noEmit`       |
| 单元     | `cargo test --lib`（含新增的额度、夹紧、信令编码用例）、`vitest run`（前端 mapper 与插值纯函数） |
| 中继 e2e | 对 CF 与自建两份中继各跑一次 `cargo test --lib pair::e2e -- --ignored`                           |
| 真机     | 双端跑 `pnpm tauri dev`（或安装包），验证 P2P 打通、回落、60Hz 视觉                              |
| 打包     | `pnpm tauri build --debug`；确认体积与编译时间变化                                               |

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

| 风险                                                  | 影响                                                                                                   | 缓解                                                                                        |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------- |
| `webrtc-rs` 在 3 个 Windows release 目标上编译失败    | Phase 8 无法交付                                                                                       | 编译 spike 已过（R24-1）；4 个非 Windows 目标已用 `cfg` 门控排除                            |
| Windows 定时器精度 15.6ms 影响 60Hz                   | 发不出真正 60Hz                                                                                        | 事件驱动而非固定 16ms 定时器                                                                |
| SCTP 消息上限（实测默认 256 KiB，不是 64 KiB）        | 附件分片在 P2P 上失败                                                                                  | 48 KiB 保守默认 + 真机大文件验证                                                            |
| `panic = "abort"` 下 webrtc 内部 panic 会带走整个 App | 一条 P2P 连接的问题升级成整个应用崩溃                                                                  | `p2p.rs` 里对 webrtc 的 `Result` 一律不许 `unwrap` / `expect`，DC / ICE 失败只走回落        |
| 自签 / 裸 IP 无法连自建中继                           | 部署文档承诺的「compose up 就能用」落空                                                                | 文档强制域名 + Caddy；可选自签开关                                                          |
| UDP 被封 / 跨境抖动                                   | P2P 打洞失败率高                                                                                       | TURN 备 TCP/443；打不通就走中继                                                             |
| STUN 暴露公网 IP 与 README 隐私承诺                   | 隐私承诺被质疑                                                                                         | 默认不填公共 STUN，设置页写明                                                               |
| `webrtc` 拉长编译与体积                               | 发布耗时、安装包变大                                                                                   | 依赖落地那一刻量一次（R24-6），必要时按 feature 门控                                        |
| 两条腿的心跳探针共用一个标志                          | 中继静默半死被 DC 流量掩盖：DC 上 pet-state 照常流动，聊天 / 信令 / 离线检测全哑却看起来正常，也不重连 | 两条独立标志——中继腿只由中继入站清除；DC 腿超时只回落（R21「心跳归属」第二条、R28 第 3 条） |

未决：自建中继是否默认广告 60 帧/秒（还是留给环境变量）。

**已定**：`webrtc-rs` 是否暴露 SCTP `max-message-size`——0.21 默认 256 KiB（`MAX_MESSAGE_SIZE`）且超限之前由实现自己分片，48 KiB 不必改（理由见 R22）。

---

# 13. Codex 执行要求

- 每次提交前用独立只读 subagent 审计，PASS 才能提交；新功能新开审计代理，复审复用同一代理。
- 规划改动先与 subagent 讨论达成一致（沿用 v1 的流程）。
- 主代理改代码，subagent 只做只读分析与审计。
- 只承诺 Windows 客户端的真机验证。
- 报告里区分静态检查、构建与真机运行三种证据。

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
> { "type": "server.welcome", "protocol": 1, "peerOnline": false,
>   "limits": { "framesPerSecond": 30.0, "chunksPerSecond": 20.0, "bytesPerSecond": 12582912.0 } }
> ```
>
> 客户端 Pacer 用广告值推导，**但不是 1:1 取用**：保持今天这组四元组不变（帧 20 速率 / 20 突发、分片 15 速率 / 10 突发，相对 CF 的 30 / 20 分别留 2/3、3/4、1/2 的余量），自建中继把额度调高时按同一比例放大；字段缺失时退回 30/20/12 MiB 的 CF 缺省值。`manager.rs:60-71` 的注释和 `manager.rs:3227-3232` 的测试都在守「严格小于中继上限」，1:1 取用会立刻复现 `close 1008`。这样自建中继可以放开到 60 帧/秒，而 CF 版拿到的缺省推导值与今天完全一致。`manager.rs` 的 `outbound_pacing_stays_within_the_relay_budget` 测试改为断言「缺省值仍在中继缺省额度内」，继续当护栏。
>
> 部署产物：`Dockerfile` + `docker-compose.yml`（relay + Caddy 自动 TLS）+ 部署文档。文档必须写清：香港节点可避开大陆 ICP 备案（大陆节点依法需备案）；1 核 1GB / 2 Mbps 够用、5 Mbps 舒服；**客户端用 `rustls-tls-webpki-roots`，必须是域名 + 受信任证书，裸 IP 或自签证书连不上**。Cloudflare 版保留为「免费方案」。

> **R21（P2P：信令钉在中继上，心跳改用应用级 Ping，能力门控对象是对端）**
>
> WebSocket 中继兼任信令通道，服务器只交换 SDP / ICE candidate。
>
> **评审修正 1（不新增帧 kind）**：中继会校验帧 kind，未知值直接 `close 1008`（`server-cloudflare/src/pair.ts:155-161`）。所以信令**复用 `FrameKind::Ping`(8)** 承载一个新的应用类型 `pair.signal`（kind 8 早就在中继的已知集合里，旧中继不会踢）。已部署的旧客户端收到未知应用类型只会走 `_` 分支忽略（`manager.rs:2509-2516`），不会崩。
>
> **评审修正 2（心跳，连发送点一起改）**：DataChannel 上**不能用** `Message::Ping`——现有判定是 `awaiting_pong && last_inbound.elapsed() >= 2×心跳`（`manager.rs:1439-1445`），而 `awaiting_pong` 只在有入站消息时清除（`1385`）。只把适配器的 Ping/Pong 写成空操作、ticker 照旧发 `Message::Ping`（`1449`）是不行的：心跳根本没发出去，`awaiting_pong` 永远不为假，对端安静两分钟就命中超时判定，`run_session` 会重连整条会话并 `abort_transfers`（在传的附件被砍）。所以**发送点必须一起改**：`1449` 的 ticker 改成入队一条应用级 `pair.ping` 帧（中继也照样转发，两端通用），WS 层 Ping/Pong 在适配器里当空操作只作兜底。响应侧有现成实现：`protocol.rs:32` 的 `FrameKind::Ping`，收到就回 pong 在 `manager.rs:2249-2256`。立即失败条件 = 适配器流报错/结束、DC 或 ICE 进入 failed/closed、`send` 失败；心跳只做兜底。
>
> **评审修正 3（切换必须在 `live` 内部完成）**：`run_session` 把 `live` 的**任何**退出都当成断线并 `abort_transfers`（`manager.rs:1116-1128`），所以不能靠「退出重进 `live`」来换传输，否则每次切换都会砍掉正在跑的附件传输。
>
> 推荐形状：`live` 继续**常读中继那条流**（`server.peer`、关闭码、信令都还在上面），只把「出站 sink」指向当前生效的传输；`SessionState`（帧序号、可靠/可覆盖队列、transfer 会话）全程不重置。信令**永远钉在中继**，重新协商前先把 active 切回中继（否则 DC 一断就没人送 SDP）。
>
> 能力门控：**只门控对端**——双方互发应用级 hello 声明 P2P 能力，只有收到对端声明才开始 ICE，否则对面是旧客户端时会白等一轮超时。`pair.signal` 走的是 kind 8，旧中继本来就原样转发，所以**不把「中继广告」当成 P2P 的前提**：`server.welcome` 里的 `limits`、`iceServers`、`capabilities` 都是可选的顶层增强字段——自建中继目前一定带 `limits`、配了 coturn 才带 `iceServers`，`capabilities` 留到 Phase 8 加 P2P 时再广告；缺失就按缺省值走。`server-cloudflare` 本次同步补这些可选字段**不是阻塞项**（列为 Phase 7b 的可选项）；没补的时候 CF 用户要在设置页手填 STUN/TURN，否则只有 host candidate（同局域网可用）。
>
> 回落策略：中继**一直保持**（它本身就是信令通道 + 离线检测，成本只是一枚 60 秒心跳），不做「失败再回落」。并行推进：一连上中继就是可用状态、状态立刻从中继发；ICE 在后台同时跑；只有 DC `open` 且应用级 ping/pong 往返成功，才把**可覆盖流**切过去。ICE 那 10~30 秒对用户不可见。切换按消息边界进行，**绝不在传输中途切**（V1 没有断点续传，接收侧要求分片序号严格递增，`transfer.rs:306-317`）；DC 中途掉线立刻回中继，当前附件按 §43 判失败。
>
> 第一阶段（Phase 8）**只让 DC 承载可覆盖流（pet state / stats）**，附件分片、聊天、控制、信令、心跳、重连全部留在中继。**附件分片绝不能走 `pet-state` 通道**（它是 `ordered=false, maxRetransmits=0`，而接收侧要求分片序号严格递增、V1 没有断点续传，丢一片或乱一片整单就废）；将来要让传输走 P2P，只能走 `reliable` 通道。这样范围比「全量替换」小得多，也天然绕开上面那个心跳陷阱。
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
> P2P 下用更小值（48 KiB 保守默认，因为 SCTP 默认消息上限 64 KiB，RFC 8841）；中继下保持 512 KiB。
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
> 1. `webrtc` crate（0.21，默认 `runtime-tokio` + `crypto-ring`）必须在 `.github/workflows/release.yml` 的**全部 7 个目标**上编译通过（含 `i686-pc-windows-msvc` 与 `aarch64-unknown-linux-gnu`）。**先做一个只验证编译的 spike**，这是最大的进度风险。
> 2. 仓库**没有 PR / 提交级别的编译或测试门禁**（`.github/workflows` 只有 `release.yml` / `sync-to-gitee.yml` / `upgradelink.yml`；`release.yml` 只在打 `v*` tag 时 `pnpm tauri build`，会编译但不跑测试）。Phase 8 是大重构，没有 CI 兜底很难受——先补一个跑 `cargo test --lib` + `pnpm test` + `tsc --noEmit` 的 job。（`server-relay/` 那一份已在 R25-7 落地，客户端侧仍留到 Phase 8。）
> 3. 自建中继必须有受信任证书（客户端用 `rustls-tls-webpki-roots`）：文档强制要求域名；「允许自签 / 自定义 CA」作为可选设置项，默认不做。
> 4. R10 的 secret 纪律要覆盖自建流程：compose 用 env 文件或 stdin 喂，**不要**写 `-e PAIR_AUTH_TOKEN=xxx`；需要一个与 `server-cloudflare/scripts/generate-pair.mjs` HKDF 等价的本地生成器（自建侧用户不装 Node）。
> 5. 香港节点避备案是对的，但 UDP 可能被封、跨境链路抖动大——这恰恰是 P2P 收益最大的场景，TURN 要备 TCP/443。
> 6. `webrtc` 会明显拉长编译时间与产物体积（7 个目标）——Phase 8 开始前先量一次。

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

| 形态 | 部署方式 | 谁维护 | 适用 |
| --- | --- | --- | --- |
| Cloudflare 版 | `wrangler` + Durable Object | 用户 A | 免费、零运维、不用域名；额度受限（30 帧/秒、每日请求数） |
| 自建版 | `docker compose up -d` | 用户 A | 完全自主；可放开到 60 帧/秒；可顺带跑 coturn |
| 裸机 / NAS | 编译 `server-relay` 直接跑 | 用户 A | 有公网 IP 的机器；**必须有域名 + 受信任证书**，否则客户端连不上 |

自建版推荐组合：香港轻量云（腾讯云 / 阿里云）→ `1 核 1GB / 2 Mbps`。带宽参考（只算中继形态的转发）：

| 场景 | 建议带宽 |
| --- | --- |
| 猫咪状态 + 文字聊天 | 1 Mbps 就够 |
| 再加语音、偶尔图片 | 2~3 Mbps |
| 图片/文件传输体验正常 | 5 Mbps |
| 经常传几十/几百 MB | 10 Mbps+ |

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

函数体只改 `transport.split()` 一行；让 DataChannel 适配器**直接收发 `tokio_tungstenite::Message`**（所有应用数据都走 `Binary`）。四个辅助函数一个字都不用动，也不需要引入内部 enum。

WS 专有帧的处理点：`1256`（主动 `Close`）、`1431-1435`（入站 Close → `describe_close`）、`1436`（`Ok(_)` 已兜住 Pong/Frame）、`1449`（心跳 Ping，见 R21）。适配器里 Ping/Pong 空操作、Close → `dc.close()`、DC 关闭 → 回 `Ok(Message::Close(None))` 或结束流。

## 4.2 传输切换

按 R21：`live` 常读中继流，出站 sink 指向当前生效传输；信令钉在中继；切换按消息边界；DC 掉线立刻回中继。日志与错误文案改中性。

---

# 5. P2P 设计

## 5.1 信令

- 新应用类型 `pair.signal`，走 `FrameKind::Ping`(8)，载荷含 `kind`（`offer` / `answer` / `candidate` / `hello`）等字段，AEAD 加密后经中继转发。
- 门控：只看**对端**——双方互发应用级 hello 声明 P2P 能力，收到对端声明才发起 ICE。旧中继不影响（`pair.signal` 走 kind 8，它本来就转发）；`server.welcome` 的 `capabilities` 只是可选增强。

## 5.2 两条 DataChannel

| 通道 | 选项 | 承载 |
| --- | --- | --- |
| `pet-state` | `ordered = false, maxRetransmits = 0` | pet state、stats（可覆盖流，丢帧不重传） |
| `reliable` | 有序可靠 | 聊天、暂离、ACK、附件分片 |

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

- 交付：`live` 泛型化 + 一个「把 WebSocket 当传输」的适配器；
- 验证：现有 `cargo test --lib`（97 passed）全绿；真中继 e2e 5 条全绿。这一步单独可审、单独可提交。

## Phase 8b：信令 + ICE + DataChannel

- 交付：`pair.signal` 应用类型、能力门控、`p2p.rs`（PeerConnection 生命周期）、`pet-state` 通道；
- 验证：真机双端打通 P2P，能看到 DC open、ping/pong 往返。

## Phase 8c：切换与回落

- 交付：可覆盖流切到 DC 的逻辑、DC 掉线回落中继；附件分片要走 `reliable`（该通道落地后再做，否则留在中继）；
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

| 层 | 手段 |
| --- | --- |
| 静态 | `cargo check --lib`、`cargo fmt --check`（注意仓库既有差异）、`eslint src`、`tsc --noEmit` |
| 单元 | `cargo test --lib`（含新增的额度、夹紧、信令编码用例）、`vitest run`（前端 mapper 与插值纯函数） |
| 中继 e2e | 对 CF 与自建两份中继各跑一次 `cargo test --lib pair::e2e -- --ignored` |
| 真机 | 双端跑 `pnpm tauri dev`（或安装包），验证 P2P 打通、回落、60Hz 视觉 |
| 打包 | `pnpm tauri build --debug`；确认体积与编译时间变化 |

新增 CI job（R24）后，静态与单元两层应在 PR 上自动跑。

---

# 12. 风险与未决

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| `webrtc-rs` 在 7 个 release 目标上编译失败 | Phase 8 无法交付 | 先做编译 spike（R24-1） |
| Windows 定时器精度 15.6ms 影响 60Hz | 发不出真正 60Hz | 事件驱动而非固定 16ms 定时器 |
| SCTP 消息上限（默认 64 KiB） | 附件分片在 P2P 上失败 | 48 KiB 保守默认 + 真机大文件验证 |
| 自签 / 裸 IP 无法连自建中继 | 部署文档承诺的「compose up 就能用」落空 | 文档强制域名 + Caddy；可选自签开关 |
| UDP 被封 / 跨境抖动 | P2P 打洞失败率高 | TURN 备 TCP/443；打不通就走中继 |
| STUN 暴露公网 IP 与 README 隐私承诺 | 隐私承诺被质疑 | 默认不填公共 STUN，设置页写明 |
| `webrtc` 拉长编译与体积 | 发布耗时、安装包变大 | Phase 8 前量一次，必要时按 feature 门控 |

未决：`webrtc-rs` 是否暴露 SCTP `max-message-size`（决定 48 KiB 是否可放宽）；自建中继是否默认广告 60 帧/秒（还是留给环境变量）。

---

# 13. Codex 执行要求

- 每次提交前用独立只读 subagent 审计，PASS 才能提交；新功能新开审计代理，复审复用同一代理。
- 规划改动先与 subagent 讨论达成一致（沿用 v1 的流程）。
- 主代理改代码，subagent 只做只读分析与审计。
- 只承诺 Windows 客户端的真机验证。
- 报告里区分静态检查、构建与真机运行三种证据。

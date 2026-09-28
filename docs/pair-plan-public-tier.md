# BongoCat 双人联机（第四阶段）设计：公益档（只借服务器打洞）

> **状态：已完成（归档，2026-09-28）**。Phase 13 已实现并提交。本文只作历史记录：以后的改动不再往这里追加修订记录（改动说明写在提交信息里），独立审计**不**以本文为参考，以当前代码、提交信息与 `AGENTS.md` 为准。

> ## 与前几份文档的关系
>
> - v1 = `docs/pair-plan.md`（Phase 1~6：多窗口 / Cloudflare Relay / 对方猫 / 聊天 / 附件 / 语音，R1~R19）。
> - v2 = `docs/pair-plan-cloud-p2p.md`（Phase 7~10：自建中继 / P2P / 60Hz / reliable 通道，R20~R33）。
> - v3 = `docs/pair-plan-multi-session.md`（Phase 11~12：多会话服务端 / 服务器密码，R34~R37）。
> - 本文 = v4（Phase 13：公益档）。**前三份的线上契约是不可动基线**：14 字节明文帧头、`AppEnvelope`、`FrameKind` 集合、HKDF 参数、`server.welcome` / `server.peer` 的形状、关闭码 `4002`~`4004` / `1008` / `1009` 全都没动；本文只**新增** `server.welcome` 里可选的 `tier`、可选请求头 `X-Bongo-Tier`、关闭码 `4005`、HTTP `409` / `429`，以及公益档自己那套配置。
> - 需求来源：用户的两句话——「别人可以部署一个 bongocat 服务器，只能给双方打洞，不能中继」；「服务器版本也要升级，加上公益密码，连接数 ip 限制等」。范围不变：客户端只做 Windows。

## 要解决的问题

服务器密码是一把**共享**钥匙：拿到它的人都能开新会话、把连接挂着占位（占满 `PAIR_MAX_SESSIONS` 之后别人拿到 503），也能拿走 `welcome` 里广告的 TURN 凭据。所以部署者只有两个选择：把密码捂严实，或者把自己的服务器白给。

用户要的是第二个选择的一个安全版本：**可以把自己的服务器开给不熟的人，但那些人只能借它打洞**——打洞成功之后数据走两台电脑之间的直连，服务器一滴都不转发；打洞失败，那就没有兜底，而不是偷偷用我的带宽。

## 设计要点

**档位跟连接走，不跟会话走。** 服务器密码与公益密码是两把不同的钥匙（配置阶段就要求两者不同），每条连接进来时按自己带的那把钥匙定档（`Tier::Full` / `Tier::Public`），记在 `Reservation.tier` 上。所以「公益密码只能打洞」是一条与别人无关的性质：拿公益密码的人永远只是公益档，哪怕他碰巧和一个拿部署者密码的人进了同一个会话——那种情况服务器直接回 **409**（这个 Room 的档位已经定死），避免「一个人有中继兜底、另一个人什么都没有」这种半截状态。

**公益档只放行 `kind 8`。** 中继能看到的只有 14 字节明文帧头，`kind` 之内（连载荷里的 type 字符串）全是 AEAD 密文。所以帧白名单只有一条规则：`kind == FRAME_KIND_SIGNAL`（8）放行，别的 kind 一律 `1008` 关连接。客户端那一侧这一类叫 `FrameKind::Ping`（名字来自最常见的用途：保活探针），协议里代表的是**信令这一类**：`pair.signal`（打洞 SDP / 候选）与 `pair.ping` / `pair.pong` 都走它。单帧上限也单独收紧：`MAX_PUBLIC_FRAME_SIZE` = 64 KiB（部署者那一档是 1 MiB）。

**这是策略与额度边界，不是密码学边界。** 服务器分不出「真信令」和「把数据塞在信令帧里」——两者的载荷对它是同一团密文。能限制的量因此全部由额度决定：`PAIR_PUBLIC_MAX_FRAMES_PER_SECOND`（默认 10）与 `PAIR_PUBLIC_MAX_BYTES_PER_SECOND`（默认 256 KiB/秒）。真正贵的东西一开始就不给：`iceServers` 在公益档会被 `stun_only()` 过滤成只剩 STUN，内置 STUN 照给。

**两套名额、两份额度，互不侵占。** 公益档有 `PAIR_MAX_PUBLIC_SESSIONS`（默认 10）与 `PAIR_MAX_PUBLIC_PER_IP`（默认 4，一条会话是两条连接，也就是两对）。每 IP 限额只挡**新建会话**：同一个 NAT 下面的一对人不会被自己挡住；同一个 Room 的第二台设备照旧进得来。公益档满了不影响部署者那一档，反过来也一样。

**「配了密码」与「开着」是两件事。** `PAIR_MAX_PUBLIC_SESSIONS=0` 是「暂时关掉这一档、密码先留着」的正当用法（不像 `PAIR_MAX_SESSIONS=0` 那样一定是配置事故），所以 0 照收、只打一条提醒。关掉之后那一档必须表现得像「这台服务器没有公益档」：凭据判定不认那把钥匙（403「服务器密码不正确」，而不是 503「会话已满」——名额根本没被占满，而 503 会让客户端按退避无限重试一台永远进不去的服务器），`/health` 的 `publicTier` 也报 `false`。判据只有一个函数（`Relay::has_public_tier`），三处共用，不会各自漂。

**每 IP 限额按哪个 IP 算。** 域名模式下 Caddy 在另一个容器里，对端地址永远是内网地址，所以真实客户端要从 `X-Forwarded-For` 取——但只在「对端本身是私网 / 回环」时才信这个头（`PAIR_TRUST_PROXY=1`，`docker-compose.yml` 默认给 1；`docker-compose.direct.yml` 保持关闭，那里对端就是客户端本身，打开只会让人伪造）。取的是 `X-Forwarded-For` 的**最后一个**，也就是 Caddy 自己加上去的那一跳。

**空闲回收 ≠ 打洞截止。** 看起来最自然的做法是「给公益连接一个 180 秒的打洞期限，到点就断」，但那条路是错的：中继看不到直连有没有打通（信令是密文），到点硬断会掐断**已经直连成功、正在正常聊天**的会话；而中继一断，客户端是整条会话重启、直连也跟着重来。所以 `PAIR_PUBLIC_WINDOW_SECS`（默认 180 秒）判的是「多久没收到**任何**入站消息」，任何入站消息都续期。诚实客户端每 60 秒发一次 WebSocket Ping，180 秒 = 三次漏拍，到点用 **4005** 关连接（客户端把它显示成普通的「正在重连」，退避后重来）。

**兼容边界要明说。** 客户端**永远**带 `X-Bongo-Tier: 1`：它自己也不知道用户填的是哪一类密码（两种密码是同一个输入框）。部署者那一档、Cloudflare 版、旧自建版都无视这个头。只有「拿公益密码连进来」时才检查它——认不得就回 **426**，客户端把 426 显示成「两边版本不一致：请把它们都升级到最新版」，正是这种情况该说的话。不用 403 是因为那会被显示成「服务器密码不对」，把人引向完全错误的方向。反过来，`server.welcome` 的 `tier` 用 `deserialize_tier` **宽容**解析：缺字段、`null`、形状不对、认不出来的取值，全部按 `full`——旧中继压根不发这个字段，而「把一个真公益档认成 full」的代价只是多一次重连（服务器会自己在收到数据帧时用 `1008` 拒绝），比「welcome 读不出来」轻得多。

## 服务端改动（`server-relay/`）

- `protocol.rs`：`HEADER_TIER` / `TIER_HEADER_VALUE` / `FRAME_KIND_SIGNAL` / `MAX_PUBLIC_FRAME_SIZE` / `Tier` 枚举 / `close_code::PUBLIC_WINDOW = 4005` / 4 个缺省常量（会话数、每 IP、帧率、字节率、空闲窗口）/ `ServerFrame::Welcome` 加 `tier`。
- `relay.rs`：`RelayOptions` 结构体（把十来项配置收成一个，主程序与集成测试都走 `Config::relay_options`，以后加一项不会出现「主程序填了、测试没填」）、`classify_server_token`（两把钥匙里认出一档）、`IpKey` + `client_ip`（含 XFF 的取法）、`stun_only`、按档位取名额与额度、公益档的帧白名单与单帧上限、空闲回收器、`TierMismatch` / `PublicIpLimit` 两种拒绝。
- `server.rs`：6 个新配置字段 + `env_bool` 严格的布尔解析（只认 `1/0`、`true/false`、`yes/no`、`on/off`，别的写法拒绝启动）+ 409 / 429 / 426 + `/health` 的 `publicTier` + `Config::relay_options`。
- `main.rs`：启动输出印出公益档的实际状态（开没开、几组、每 IP 几组、额度、真实 IP 从哪来）——部署者一眼能看出自己配对了没有。

## 客户端改动（`src-tauri/src/core/pair/`）

- `protocol.rs`：`RelayTier` 枚举 + `is_public()`、`TIER_HEADER_VALUE`、`FrameKind::is_signal()`、`ServerFrame::Welcome.tier`（宽容解析）、`RelayConfig.tier`。
- `client.rs`：`build_request` 永远发 `X-Bongo-Tier`；`describe_connect_error` 加 409（两边填了不同类型的密码，fatal）与 429（公益名额满，transient）。
- `manager.rs`：`PairStatus.tier` 进前端；`manual_blocked` 改名并推广成 **`outbound_blocked`**（配对码 或 公益档 + 直连没建立）；`send_chat` / `stage_attachment` / `retry_attachment` 都过它；`SessionState.public_tier` + reliable 队列改成 `QueuedFrame`；`describe_close` 加 4005。
- **拿不出去的那一帧要交回上层，不能静默丢**（独立审计抓出来的 P1，同一个改动里的三处收尾必须一致）：`take_non_signal()`（拿到 `welcome` 时把「档位还不知道」那段时间攒下的非信令帧挑出来）、`flush()`（发送时这条帧不能落到中继上）、以及队列满时的挤占，三者都走 [`retry_dropped_chat`]：聊天退回「等待发送」（§32，对方下次上线/直连恢复时补发）、附件 offer 判失败并收掉会话（否则永远等不到 accept 的那一单会白占名额）。不这么做的话本地那条消息已经被标成「已发送」，中继那一侧又不会替它兜底，消息就永远不到、也不会被补发。
- `flush()` 还要分清「这条腿**不可用**」与「这条腿在、只是正被**背压**挡住」：前者交回上层，后者把帧放回队头等下一轮（对面正在收大文件时把聊天判成「待发送」是错的）。直连那条可靠腿**刚刚**被验证可用时顺手 `resend_pending_chat()`——公益档下退回等待发送的消息正好在这一刻有地方可发。
- 档位是**这一次连接**的事实：`start()` / `start_manual_offer()` / `disconnect()` 都把它复位成 `full`，否则「从公益服务器断开」之后界面还会说「等直连建立就能发」，而真实情况是「还没连上服务器」。

## 前端改动（`src/`）

- `stores/pair.ts`：`outboundBlockKey`（与 Rust 同一套判据，返回 i18n key）、`recordingBlockReasonKey` 的公益档分支；`runtime.tier`。
- `pages/chat/index.vue` 与 `components/chat-overlay/index.vue`：发送键与麦克风都改成按 `outboundBlockKey` 挡，并把原因显示出来。
- `pages/main/index.vue`：`canSendRecording` 改成复用同一份原因（`recordingBlockReason`），两处不再各写一份判据。
- `pages/preference/components/pair/index.vue`：P2P 那一行加「公益档」标签，并换掉那句话——默认那句「失败会自动回退到服务器，功能不变」在公益档是**假的**。
- `composables/usePair.ts` / `usePairStatus.ts`：`PairTier` 类型与 `tier` 的回落（关掉联机时落回 `full`，免得「上次连的是公益服务器」这条记忆在断开之后还灰着发送口）。

## 验证

- `server-relay`：`cargo test`（56 单元 + 35 集成 + 真实 WebSocket 端到端）全绿，`cargo clippy --all-targets` 无 warning。新增用例覆盖：公益档被正确公告、公益连接转发不了数据帧、公益档要求认得档位的客户端（426）、公益档不占部署者名额、每 IP 限额只挡新会话、两类密码混用一个 Room 回 409、超大公益帧 `1009`、空闲超时 `4005`、关掉时 `/health` 报 `false` 且那把钥匙 403、开着时报 `true`、以及「部署者那一档完全没被动过」。
- 客户端：`cargo test --lib` 170 passed。新增用例覆盖：公益档在直连建立之前挡住数据、信令照常通过、拿不出去的数据帧交回上层（而不是留在队列里）、背压时等下一轮、直连建立之后一切照旧、`take_non_signal` 只留信令。
- 前端：`node node_modules/eslint/bin/eslint.js src`、`node node_modules/typescript/bin/tsc --noEmit`、`node node_modules/vitest/vitest.mjs run` 全绿（`stores/pair.spec.ts` 新增 `outboundBlockKey` 四种组合与公益档分支）。
- 顺手修掉一个既有的 flaky 用例：`src-tauri/src/core/pair/manual.rs` 的 `a_tampered_code_is_rejected` 改最后一位 base64 填充时有约 1/6 的概率改出一个等价的码，改成改倒数第 6 位。
- 本地实跑（不是打包）：直接起 `server-relay` 的二进制三次，核对启动输出（`PAIR_MAX_PUBLIC_SESSIONS=0` 报「实际关闭」并打提醒；默认报「已开启…最多 10 组、每 IP 4 组…」；`PAIR_TRUST_PROXY=maybe` 拒绝启动并说清只认哪几种写法）。

## 已知边界

- 公益档不解决「占位」：它有自己的名额，也可能被挂满（满了回 429，客户端按退避重试）。想更严就在前置网关上限 IP 或限速。
- 每 IP 限额依赖 `X-Forwarded-For` 的正确性，只在「对端是私网 / 回环 + `PAIR_TRUST_PROXY=1`」时生效；direct 模式没有反代，所以它按对端地址算，本来就是真实 IP。
- 两种模式常常共用一份 `.env`，所以 `docker-compose.direct.yml` 把 `PAIR_TRUST_PROXY` **写死 0**：不然按 `.env.example` 的注释在域名模式打开过它的人，切到 direct 模式后会允许**私网**对端伪造 `X-Forwarded-For`，把每 IP 限额绕过去。
- Cloudflare 版**不做**公益档：一个部署只服务一对用户，而且那边跑不了 UDP，没有可借的打洞能力。
- TURN 凭据仍然只发给部署者那一档；公益档要更硬的做法（按会话签发限时凭据）属于以后的事。

# BongoCat 双人联机（第三阶段）设计：一套服务器承载多个双人会话

> ## 与前两份文档的关系
>
> - v1 = `docs/pair-plan.md`（Phase 1~6：多窗口 / Cloudflare Relay / 对方猫 / 聊天 / 附件 / 语音，R1~R19）。
> - v2 = `docs/pair-plan-cloud-p2p.md`（Phase 7~10：自建中继 / P2P / 60Hz / reliable 通道 / 单机验收，R20~R33）。
> - 本文 = v3（Phase 11：多会话服务端）。**v1 与 v2 的线上契约是不可动基线**：14 字节明文帧头（帧头同时是 AEAD 的 associated data）、`AppEnvelope`、`FrameKind` 集合、R17 的 HKDF 参数、`server.welcome` / `server.peer` 的形状、关闭码——本轮一律不动。
> - 冲突时以本文为准；本文只描述本轮**新增**的能力。v1 / v2 的章节继续有效。
> - 需求来源：用户提供的「多会话双人联机服务端重构方案」（37 节）。方向照单采纳，实现细节按下面的修订记录落地（**每一处偏离都在 R34 里写明**）。
> - 范围不变：客户端只做 Windows；`server-relay/` 本身跨平台，但只承诺 Linux + Docker。
>
> ## 修订记录（R34 起）
>
> R34 是本轮的实现记录，共 11 条；其中**确实与原方案不同**的是第 5、6、9、10 条，以及「部署」一段里的 ①②③，合计 7 处（11 条里其余 7 条是照方案做的记录）。R35 是提交前的只读独立审计结论。

> **R34（Phase 11 落地：Room 化的自建中继 + 客户端 Room 头 + 设置页与部署简化）**
>
> 核心改动只有一条：`server-relay` 从「一套服务器实例绑定一个 Pair Secret、只允许一对用户」改成「一套服务器同时承载多个 Pair Secret 对应的双人会话」。CF 版一行没动（它继续「一个部署 = 一对用户」，并忽略新头）。
>
> **1. Room 的派生与固定向量**（`src-tauri/src/core/pair/crypto.rs`、`server-relay/src/auth.rs`）
>
> ```text
> PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-room-v1")--> ROOM_ID    (32B -> base64url 无填充，43 字符)
> PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-auth-v1")--> PAIR_AUTH_TOKEN   (既有)
> PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-e2ee-v1")--> E2EE_ROOT_KEY     (既有)
> ```
>
> 三路输出彼此独立，**不用 `SHA256(secret)` 直接当 Room**。固定向量：`AUTH_TOKEN` 与根密钥那两条与 TS 侧（`server-cloudflare/test/hkdf-vector.spec.ts`）是同一条跨语言向量，`ROOM_ID` 那一条是两份 Rust crate（客户端 `crypto.rs` 与中继 `auth.rs`）各写一遍（TS 侧没有 Room 派生）——任何一侧漂移都会被另一侧抓住：
>
> ```text
> secret  AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8
> room    r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M
> ```
>
> **2. 只在升级头里加分组键，不动协议版本**：客户端多发一个 `X-Bongo-Room: <ROOM_ID>`，`PROTOCOL_VERSION` / `SIGNAL_VERSION` 仍是 1，`FrameHeader` / `AppEnvelope` / `FrameKind` / AEAD 一行不改。兼容矩阵：**新客户端**三边都能连（新自建 / 旧自建 / CF，后两者忽略未知头）；**旧客户端**连新自建会被 **HTTP 400** 挡住（它没有分组键，见第 6 条），连 CF 照旧。所以自建管理员要**先升级两台设备上的客户端，再升级服务器**（`server-relay/README.md` 里有同一张表）。
>
> **3. 服务端不再有任何联机密钥**：`PAIR_SECRET` / `PAIR_AUTH_TOKEN` 两项配置被删除，`Config` 变成「限流额度 + 会话上限 + 陈旧判定 + ICE」。中继只存 `SHA256(AUTH_TOKEN)` 当 verifier，用恒定时间比较（`auth::constant_time_eq`）；它拿不到原始 secret，也拿不到 E2EE 根密钥（§4 的承诺）。
>
> **4. 容量按「会话数」算，而不是连接数**（`PAIR_MAX_SESSIONS`，默认 20）：满员只拒绝**新建会话**（HTTP 503 `server capacity reached`），已有会话的第二个人加入、同 deviceId 重连都不受影响；一组人全退出后名额立刻释放。
>
> **5. 名额的账必须两边对得上（原方案没写机制，这里补上）**：容量判定发生在 WebSocket 升级**之前**（客户端要靠状态码区分 401 / 503），但握手本身可能失败。所以 `reserve()` 在放行时当场创建 Room 并把它的 `pending` 计数 +1，`admit()` 消费它，握手失败走 `release()`。`sweep()` 只在 `clients` 为空**且** `pending == 0` 时删除 Room——少了 `pending`，「握手失败」会漏名额、「握手中」会被误删（后者会让随后到达的 `admit` 落进「Room 不存在」的分支，账就乱了）。
>
> **6. HTTP 阶段的判定顺序**（`server.rs`）：`X-Bongo-Room` 格式（400）→ Authorization 非空（401）→ Room verifier 与容量（401 / 503）→ `deviceId` 格式（400）→ 升级。
>
> 这里有一处**语义变化必须写明**：v2 的「先鉴权再看 deviceId」是为了不让未鉴权的人探测 deviceId 合法性；多会话下「鉴权」= 「与已存在的 Room 的 verifier 一致」，而**新建**会话天然没有共享凭据可验。所以这条保证现在只对**已存在的会话**成立（集成用例 `rejects_bad_auth_protocol_and_device_ids` 就是按这个口径写的）。
>
> **7. 所有跨连接的动作都先落进 Room**：binary 转发、`server.peer` 上下线、同 deviceId 顶替、陈旧连接摘除、停摆对端剔除、`forward()` 的目标集合。任何一处遍历全局连接都会串房，所以单测里对隔离性全做**负向断言**（B 房收不到 A 房的帧 / 公告）。
>
> **8. 断开竞态按 `(room, deviceId, connectionId)` 三重匹配**（§14）：同一个 deviceId 的新连接顶掉旧连接之后，旧连接读循环的迟到清理必须被认出来并忽略——否则「新连接刚连上就被判离线」。单测 `a_late_cleanup_from_a_replaced_connection_keeps_the_new_one` 守这条。
>
> **9. 顶替谁必须确定性**：CF 版按 Durable Object 的 WebSocket **插入顺序**找第一个陈旧连接；本实现的 `clients` 是 `HashMap<deviceId, _>`，迭代顺序随机，所以改成「取 id 最小的那个」（连接 id 由 `AtomicU64` 单调发下去，与插入顺序等价）。不这么做，「顶替谁」会变成抽签——这条是原方案没考虑到的实现细节。
>
> **10. 关闭码与错误文案**：`4002` / `4003` / `4004` / `1008` / `1009` / `1011` 全部沿用；`4003` 的 reason 与 CF 版**逐字一致**（`pair is full`，集成用例连 reason 一起断言）——多会话之后「这一桌满了」在语义上仍然成立，两边说同一句话能省掉一次「为什么这边说的是 `room`」的排查。新增的「服务器满员」走 **HTTP 503**（CF 版永远不会返回它，这是契约上的第四处差异），客户端翻译成「服务器双人联机会话已满，请稍后再试」，并按**可重试**处理（名额由别的会话释放，退避重连就能自己进去，用户不必手动点「立即连接」）；`4003` 翻译成「该联机会话已有两台设备在线」，仍是 fatal（同一对还占着两位，重连不会变好）；`401` 翻译成「联机密钥不正确：请与对方核对是否完全相同」。`/health` 增加可选字段 `"mode":"multi-pair"`（§28 允许），不暴露任何会话信息。
>
> **11. 日志隐私**（§29）：日志只写 `deviceId` 与**会话指纹**（`SHA256(ROOM_ID)` 前 8 字节 hex）；完整 `ROOM_ID`、`AUTH_TOKEN`、联机密钥、聊天内容一律不写。
>
> **客户端侧**
>
> - `SessionConfig` 增加 `room_id`（从保存的联机密钥派生，**不持久化第二份**）；`client::connect` 增加 `room_id` 参数并始终带 `x-bongo-room`；Room 非法（空 / 字符集 / 长度）一律 fatal，不带着非法值去连。
> - `build_upgrade_url` 按 §23 补齐：没有 scheme 时域名 → `wss://`，**裸 IP 与本机地址（`localhost`）→ `ws://`**（自建 direct 模式没有证书，猜 `wss://` 只会给用户一个看不懂的 TLS 错误）；显式 scheme 一律尊重。配套的 `is_plaintext_endpoint` 通过 `PairStatus.plaintext` + `PairStatus.relayUrl` 暴露给界面。
> - 设置页（§1 / §21 / §22）：文案改为「服务器地址」「联机密钥」（**用户可见的错误文案也一并改词**，`PAIR_SECRET` 只留作内部标识），新增「生成联机密钥」（Rust 侧 CSPRNG 32 字节，**不用** `Math.random` / 时间戳 / UUID）与「复制」；生成后顺手复制一次（保存会清空输入框，不然「生成 → 保存 → 再复制」走不通）；地址是明文时显示一条**非阻塞**提醒，绝不阻止连接，而且**只在提醒说的地址和输入框当前值一致时**才显示（Rust 侧只在连接那一刻算这个值，改地址不会把它清掉）。
>
> **部署（§25 / §26）**
>
> - `.env.example` 删掉 `PAIR_SECRET`，只留容量 / 额度 / ICE / 域名；`docker-compose.yml` 不再注入任何密钥。
> - 新增 `docker-compose.direct.yml`：裸 IP 直接 `8080:8080`，无 TLS，README 明确提醒。
> - `generate-pair` 不再写 `.env`、也不再是「服务器配置」的一步：它现在只是「命令行生成一个联机密钥」的便利工具，打印密钥 + 核对指纹 + 会话指纹。服务器端不再需要它。
> - 三处与原方案的偏离：① `PAIR_MAX_SESSIONS=0` 直接启动报错（否则是一个「永远 503」的部署，几乎一定是配置事故）；② `server::serve` 不再接收 `Config`（每一项都已折进 `Relay`，不留第二份真相）；③ 客户端对 Room 长度按**定长 43** 校验（它是自己派生出来的，定长能立刻抓到派生漂移），服务端按「非空 / ≤64 / `[A-Za-z0-9_-]`」的上界校验。
>
> **测试**（§30 / §31 / §32）：`server-relay` 单测 37 条（含 disconnect race、握手中的预留与归还、Room 隔离的负向断言）+ 集成 20 条（真实 WebSocket 上的 Room 隔离、名额释放、满员 503、错 token 401）；客户端新增 Room 派生向量、Room 头必带、非法 Room、URL 归一化（含 `localhost`）、明文判定，以及 §31 点名的**三条用户可见文案**（401 / 503 / 4003，含「哪个 fatal、哪个可重试」）；真中继多会话端到端 `two_rooms_share_one_relay_without_crossing`（四个 `PairManager`、两份密钥、双向负向断言，并且**两组各自的 DataChannel 都要打通**——信令串房时这条腿立不起来）。

> **R35（提交前的只读独立审计）**
>
> 四个互不共享上下文的只读审计代理（客户端 / 界面 / `server-relay` / 方案符合性）在提交前逐条复核了原方案 §1~§37，反复多轮：**每一轮上报的问题都在下一轮被独立复审**，没有一条是「改完自己说好了」；全程 **0 个 P0**。四个代理合计：第一轮 2 个 P1 + 16 个 P2。那两条 P1 是：
>
> 1. **§31 点名的三条用户可见文案（401 / 503 / 4003）原本只有实现、没有测试** → 补 `client.rs::http_status_codes_become_readable_messages`（含 400 / 426 / 404 / 未知码与「哪个 fatal」）与 `manager.rs::close_codes_become_readable_messages`。
> 2. **§21 要求改词的地方仍有 7 条用户可见错误写着 `Pair Secret`** → 全部改成「联机密钥」（`crypto.rs` 三条、`manager.rs` 一条、`secret.rs` 三条）。
>
> 其余按类别处置（每一条都在复审里被独立核对过）：
>
> - **契约一致性**：`4003` 的 reason 改回与 CF 版逐字一致的 `pair is full`，并补一条「码与 reason 一起断言」的集成用例（CF 侧本来就断言了 reason）。
> - **503 语义**：从 fatal 改成**可重试**，与「请稍后再试」一致（名额由别的会话释放，退避重连就能自己进去）。
> - **§32 的 P2P 那一支**：补进多 Room 端到端用例（等四条 DataChannel 各自打通），并在本机自建中继上实跑通过。
> - **隔离性断言加强**：跨会话的帧在接收侧**解不开**，只会进 `errors()` 而不是事件——所以负向断言同时断「事件计数」与「没有解密失败 / 未知帧类型」，否则真串房会被漏掉。
> - **界面**：明文提醒与它描述的那个地址绑定（改地址后不再残留）；「生成联机密钥」顺手复制一次（保存会清空输入框，否则「生成 → 保存 → 再复制」走不通），复制失败不再被报成生成失败；补一句「把服务器地址和密钥两个值一起发给对方」。
> - **文案**：用户可见的错误串不再出现 `deviceId` / `protocol 1` / `transferId` / 「中继」（统一说「服务器」）。注释里同步了措辞、日志一个字没动——改词没有损失任何诊断信息（关闭码、`{protocol}`、`{code}: {message}` 原文都还在）。
> - **部署与文档**：README 增加兼容矩阵（旧客户端连自建版会 400，要先升客户端再升服务器）与容量说明（`PAIR_MAX_SESSIONS` 调小不挡占位）；`Cargo.toml` / `.dockerignore` / `main.rs` 的残留清理；三份设计文档口径统一。
>
> **遗留（非阻塞，审计双方都同意留到以后）**：`client.rs` 有几条把底层英文错误原文拼进 `lastError` 的诊断文案（只在真实故障时出现，保留原文更有助于定位）；`client.rs` 的「联机会话标识含非法字符」是不可达分支；`pt-BR` / `vi-VN` / `zh-TW` 三套语言从来没有 pair 这组键（从 Phase 1 起如此，运行时走 i18n 回退）。
>
> 提交前最后一次全跑的证据：`server-relay` 单测 **37** + 集成 **20**；`src-tauri` **133 passed / 9 ignored**；真实中继端到端 **8/8**（含多 Room 与其中的 P2P）；`eslint` / `tsc --noEmit` / `vitest`（51 条）/ `cargo clippy --all-targets`（server-relay）全绿。`cargo fmt --check`：`server-relay` 干净；`src-tauri` 是**既有**的 37 处差异（`HEAD` 同样是 37 处、逐文件一致，本次改动零新增）。

# 1. 目标与非目标

## 1.1 目标

- 普通用户只需要两个值：**服务器地址** + **联机密钥**。两个人填一样的值就进入同一个双人会话。
- 一套服务器同时承载 `PAIR_MAX_SESSIONS` 组互不可见的双人会话。
- 每个会话最多两台不同设备；第三台被明确拒绝。
- 会话、公告、信令、聊天、附件、桌宠状态、统计**一个字节都不能串房**。
- 既有能力零回归：WebRTC / DataChannel / 60Hz / reliable 通道 / 回落 / E2EE / 同 deviceId 顶替 / 陈旧顶替。
- 服务器仍然不知道：原始联机密钥、E2EE 根密钥、消息明文。

## 1.2 非目标（明确不做）

用户账号、注册登录、云数据库、Redis、Room 持久化、好友系统、群聊、超过两个人的 Room、服务器保存聊天记录、服务器解密内容、重写 P2P、重写 transfer、改 `FrameHeader` / `AppEnvelope` / E2EE 算法、改 CF 版为多会话、引入新的 protocol version。

# 2. Room 模型

```text
                    BongoCat Relay
                 PAIR_MAX_SESSIONS=20
                          |
          +---------------+---------------+ ...
          |               |
       Room A          Room B
      Secret A        Secret B
       /    \          /    \
     A1      A2      B1      B2
       \    /          \    /
       WebRTC          WebRTC
         P2P             P2P
```

服务端状态（`server-relay/src/relay.rs`）：

```rust
struct State { rooms: HashMap<String, PairRoom>, buckets: HashMap<u64, Bucket> }
struct PairRoom { auth_hash: [u8; 32], clients: HashMap<String, ClientEntry>, pending: usize, created_at: Instant, last_active: Instant }
struct ClientEntry { id: u64, sender: mpsc::Sender<Message>, last_seen: Instant, ejected: oneshot::Sender<()> }
```

- `clients` 按 `deviceId` 索引：同 deviceId 重连天然是「替换掉原来那条」。
- `buckets` 仍按连接 id 全局索引（限流是按 socket 的，与分组无关）。
- 会话的生命周期：第一个客户端连接 → 创建；某一方离线 → 保留；最后一方离线（且没有握手中的连接）→ 删除并释放名额；服务器重启 → 全部消失（允许）。

# 3. 线上契约

`GET /ws` 的四个头：

```http
Authorization: Bearer <PAIR_AUTH_TOKEN>
X-Bongo-Room: <ROOM_ID>          # 新增；旧 CF 部署忽略它
X-Bongo-Client: <deviceId>
X-Bongo-Protocol: 1
```

| 状态码 | 含义 |
| --- | --- |
| 400 | `ROOM_ID` 或 `deviceId` 非法 |
| 401 | 同一会话上摘要对不上，或没带 `Authorization` |
| 503 | 服务器会话已满（**只针对新会话**） |
| 426 | 协议不支持 / 不是 WebSocket 升级 |
| 404 | 路径不对 |

`GET /health` → `{"ok":true,"protocol":1,"mode":"multi-pair"}`（鉴权之前返回，不含任何会话信息）。

关闭码沿用 v2：`4002` 顶替、`4003` 会话已满两台设备、`4004` 陈旧顶替、`1008` 协议/限流、`1009` 单帧过大、`1011` 内部错误。

# 4. 客户端改动

- `crypto::derive_room_id`（固定向量）。
- `SessionConfig.room_id`；`client::connect(relay_url, room_id, auth_token, device_id)`。
- `client::build_upgrade_url` / `is_plaintext_endpoint`（裸 IP 与本机地址 `localhost` → `ws://`）。
- `PairStatus.plaintext` + `PairStatus.relayUrl`（只用于界面提醒，并且只在提醒说的地址 == 输入框当前值时显示）。
- 命令 `pair_generate_secret`（CSPRNG 32 字节）。
- 设置页：服务器地址 / 联机密钥 / 生成 / 复制 / 连接 + 明文提醒。

# 5. 测试与验收

必须满足：

1. 全新用户只知道「服务器地址 + 联机密钥」也能联机（设置页文案与流程）。
2. 同一个密钥最多两台设备；第三台被明确拒绝（4003 → 「该联机会话已有两台设备在线」）。
3. 一套服务器同时跑 `PAIR_MAX_SESSIONS` 组互不可见的会话（负向断言：帧、公告都不串）。
4. 满员只挡新会话；已有会话不受影响；会话空了释放名额。
5. P2P / 60Hz / reliable / 回落行为零回归。
6. E2EE 承诺不变（服务器拿不到 secret / 根密钥 / 明文）。
7. CF 版继续工作（忽略新头，同一个 URL 继续可用）。
8. 服务器管理员不再配置任何密钥。

# 6. 实现记录（提交）

| Phase | 内容 | 提交 |
| --- | --- | --- |
| A | `derive_room_id` + 固定向量 | 见下 |
| B | 客户端 `X-Bongo-Room` + `SessionConfig.room_id` | 见下 |
| C/D/E | `server-relay` 多 Room + 隔离 + 容量 | 见下 |
| F | 设置页文案 + 生成联机密钥 + 明文提醒 | 见下 |
| G | 部署简化（direct compose / .env.example / README） | 见下 |

（提交哈希在推送后回填。）

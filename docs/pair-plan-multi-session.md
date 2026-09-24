# BongoCat 双人联机（第三阶段）设计：一套服务器承载多个双人会话

> ## 与前两份文档的关系
>
> - v1 = `docs/pair-plan.md`（Phase 1~6：多窗口 / Cloudflare Relay / 对方猫 / 聊天 / 附件 / 语音，R1~R19）。
> - v2 = `docs/pair-plan-cloud-p2p.md`（Phase 7~10：自建中继 / P2P / 60Hz / reliable 通道 / 单机验收，R20~R33）。
> - 本文 = v3（Phase 11：多会话服务端；Phase 12：服务器密码）。**v1 与 v2 的线上契约是不可动基线**：14 字节明文帧头（帧头同时是 AEAD 的 associated data）、`AppEnvelope`、`FrameKind` 集合、R17 的 HKDF 参数、`server.welcome` / `server.peer` 的形状、关闭码——两轮都没有动过。
> - 冲突时以本文为准；本文只描述本轮**新增**的能力。v1 / v2 的章节继续有效。
> - 需求来源：用户提供的「多会话双人联机服务端重构方案」（37 节）。方向照单采纳，实现细节按下面的修订记录落地（**每一处偏离都在 R34 里写明**）。
> - 范围不变：客户端只做 Windows；`server-relay/` 本身跨平台，但只承诺 Linux + Docker。
>
> ## 修订记录（R34 起）
>
> R34 是 Phase 11 的实现记录，共 11 条；其中**确实与原方案不同**的是第 5、6、9、10 条，以及「部署」一段里的 ①②③，合计 7 处（11 条里其余 7 条是照方案做的记录）。R35 是 Phase 11 提交前的只读独立审计结论。**R36 是 Phase 12（服务器密码）的实现记录**，它来自用户自己的一句话需求（「用户需要填写 3 个：服务器地址、服务器密码、配对密码；服务器密码防止别人白嫖服务器，这个密码在服务器上部署时设置」），触发自 Phase 12 独立审计在部署侧发现的两条 P1（陌生人能白拿 TURN 凭据、陌生人能占满会话）。**R37 是 Phase 12 提交前的只读独立审计与修复**。

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
> **3. 服务端不再有任何配对密码**：`PAIR_SECRET` / `PAIR_AUTH_TOKEN` 两项配置被删除，`Config` 变成「限流额度 + 会话上限 + 陈旧判定 + ICE」。中继只存 `SHA256(AUTH_TOKEN)` 当 verifier，用恒定时间比较（`auth::constant_time_eq`）；它拿不到原始 secret，也拿不到 E2EE 根密钥（§4 的承诺）。
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
> **10. 关闭码与错误文案**：`4002` / `4003` / `4004` / `1008` / `1009` / `1011` 全部沿用；`4003` 的 reason 与 CF 版**逐字一致**（`pair is full`，集成用例连 reason 一起断言）——多会话之后「这一桌满了」在语义上仍然成立，两边说同一句话能省掉一次「为什么这边说的是 `room`」的排查。新增的「服务器满员」走 **HTTP 503**（CF 版永远不会返回它，这是契约上的第四处差异），客户端翻译成「服务器双人联机会话已满，请稍后再试」，并按**可重试**处理（名额由别的会话释放，退避重连就能自己进去，用户不必手动点「立即连接」）；`4003` 翻译成「该联机会话已有两台设备在线」，仍是 fatal（同一对还占着两位，重连不会变好）；`401` 翻译成「配对密码不正确：请与对方核对是否完全相同」。`/health` 增加可选字段 `"mode":"multi-pair"`（§28 允许），不暴露任何会话信息。
>
> **11. 日志隐私**（§29）：日志只写 `deviceId` 与**会话指纹**（`SHA256(ROOM_ID)` 前 8 字节 hex）；完整 `ROOM_ID`、`AUTH_TOKEN`、配对密码、聊天内容一律不写。
>
> **客户端侧**
>
> - `SessionConfig` 增加 `room_id`（从保存的配对密码派生，**不持久化第二份**）；`client::connect` 增加 `room_id` 参数并始终带 `x-bongo-room`；Room 非法（空 / 字符集 / 长度）一律 fatal，不带着非法值去连。
> - `build_upgrade_url` 按 §23 补齐：没有 scheme 时域名 → `wss://`，**裸 IP 与本机地址（`localhost`）→ `ws://`**（自建 direct 模式没有证书，猜 `wss://` 只会给用户一个看不懂的 TLS 错误）；显式 scheme 一律尊重。配套的 `is_plaintext_endpoint` 通过 `PairStatus.plaintext` + `PairStatus.relayUrl` 暴露给界面。
> - 设置页（§1 / §21 / §22）：文案改为「服务器地址」「配对密码」（**用户可见的错误文案也一并改词**，`PAIR_SECRET` 只留作内部标识），新增「生成配对密码」（Rust 侧 CSPRNG 32 字节，**不用** `Math.random` / 时间戳 / UUID）与「复制」；生成后顺手复制一次（保存会清空输入框，不然「生成 → 保存 → 再复制」走不通）；地址是明文时显示一条**非阻塞**提醒，绝不阻止连接，而且**只在提醒说的地址和输入框当前值一致时**才显示（Rust 侧只在连接那一刻算这个值，改地址不会把它清掉）。
>
> **部署（§25 / §26）**
>
> - `.env.example` 删掉 `PAIR_SECRET`，只留容量 / 额度 / ICE / 域名；`docker-compose.yml` 不再注入任何密钥。
> - 新增 `docker-compose.direct.yml`：裸 IP 直接 `8080:8080`，无 TLS，README 明确提醒。
> - `generate-pair` 不再写 `.env`、也不再是「服务器配置」的一步：它现在只是「命令行生成一个配对密码」的便利工具，打印密钥 + 核对指纹 + 会话指纹。服务器端不再需要它。
> - 三处与原方案的偏离：① `PAIR_MAX_SESSIONS=0` 直接启动报错（否则是一个「永远 503」的部署，几乎一定是配置事故）；② `server::serve` 不再接收 `Config`（每一项都已折进 `Relay`，不留第二份真相）；③ 客户端对 Room 长度按**定长 43** 校验（它是自己派生出来的，定长能立刻抓到派生漂移），服务端按「非空 / ≤64 / `[A-Za-z0-9_-]`」的上界校验。
>
> **测试**（§30 / §31 / §32）：`server-relay` 单测 37 条（含 disconnect race、握手中的预留与归还、Room 隔离的负向断言）+ 集成 20 条（真实 WebSocket 上的 Room 隔离、名额释放、满员 503、错 token 401）；客户端新增 Room 派生向量、Room 头必带、非法 Room、URL 归一化（含 `localhost`）、明文判定，以及 §31 点名的**三条用户可见文案**（401 / 503 / 4003，含「哪个 fatal、哪个可重试」）；真中继多会话端到端 `two_rooms_share_one_relay_without_crossing`（四个 `PairManager`、两份密钥、双向负向断言，并且**两组各自的 DataChannel 都要打通**——信令串房时这条腿立不起来）。

> **R35（提交前的只读独立审计）**
>
> 四个互不共享上下文的只读审计代理（客户端 / 界面 / `server-relay` / 方案符合性）在提交前逐条复核了原方案 §1~§37，反复多轮：**每一轮上报的问题都在下一轮被独立复审**，没有一条是「改完自己说好了」；全程 **0 个 P0**。四个代理合计：第一轮 2 个 P1 + 16 个 P2。那两条 P1 是：
>
> 1. **§31 点名的三条用户可见文案（401 / 503 / 4003）原本只有实现、没有测试** → 补 `client.rs::http_status_codes_become_readable_messages`（含 400 / 426 / 404 / 未知码与「哪个 fatal」）与 `manager.rs::close_codes_become_readable_messages`。
> 2. **§21 要求改词的地方仍有 7 条用户可见错误写着 `Pair Secret`** → 全部改成「配对密码」（`crypto.rs` 三条、`manager.rs` 一条、`secret.rs` 三条）。
>
> 其余按类别处置（每一条都在复审里被独立核对过）：
>
> - **契约一致性**：`4003` 的 reason 改回与 CF 版逐字一致的 `pair is full`，并补一条「码与 reason 一起断言」的集成用例（CF 侧本来就断言了 reason）。
> - **503 语义**：从 fatal 改成**可重试**，与「请稍后再试」一致（名额由别的会话释放，退避重连就能自己进去）。
> - **§32 的 P2P 那一支**：补进多 Room 端到端用例（等四条 DataChannel 各自打通），并在本机自建中继上实跑通过。
> - **隔离性断言加强**：跨会话的帧在接收侧**解不开**，只会进 `errors()` 而不是事件——所以负向断言同时断「事件计数」与「没有解密失败 / 未知帧类型」，否则真串房会被漏掉。
> - **界面**：明文提醒与它描述的那个地址绑定（改地址后不再残留）；「生成配对密码」顺手复制一次（保存会清空输入框，否则「生成 → 保存 → 再复制」走不通），复制失败不再被报成生成失败；补一句「把服务器地址和密钥两个值一起发给对方」。
> - **文案**：用户可见的错误串不再出现 `deviceId` / `protocol 1` / `transferId` / 「中继」（统一说「服务器」）。注释里同步了措辞、日志一个字没动——改词没有损失任何诊断信息（关闭码、`{protocol}`、`{code}: {message}` 原文都还在）。
> - **部署与文档**：README 增加兼容矩阵（旧客户端连自建版会 400，要先升客户端再升服务器）与容量说明（`PAIR_MAX_SESSIONS` 调小不挡占位）；`Cargo.toml` / `.dockerignore` / `main.rs` 的残留清理；三份设计文档口径统一。
>
> **遗留（非阻塞，审计双方都同意留到以后）**：`client.rs` 有几条把底层英文错误原文拼进 `lastError` 的诊断文案（只在真实故障时出现，保留原文更有助于定位）；`client.rs` 的「联机会话标识含非法字符」是不可达分支；`pt-BR` / `vi-VN` / `zh-TW` 三套语言从来没有 pair 这组键（从 Phase 1 起如此，运行时走 i18n 回退）。
>
> 提交前最后一次全跑的证据：`server-relay` 单测 **37** + 集成 **20**；`src-tauri` **133 passed / 9 ignored**；真实中继端到端 **8/8**（含多 Room 与其中的 P2P）；`eslint` / `tsc --noEmit` / `vitest`（51 条）/ `cargo clippy --all-targets`（server-relay）全绿。`cargo fmt --check`：`server-relay` 干净；`src-tauri` 是**既有**的 37 处差异（`HEAD` 同样是 37 处、逐文件一致，本次改动零新增）。

> **R36（Phase 12 落地：服务器密码 + 三项凭据的设置页与部署）**
>
> 一句话：**「能不能用这台服务器」与「谁是同一对」是两件事**。之前只有后者有凭据（配对密码），于是任何知道地址的人都能开一个自己的会话、把连接挂着占位，并且顺手拿走 `server.welcome` 里的 TURN 凭据（那是按流量计费的）。这一轮把前者补成一个**必填**的服务器门槛。
>
> **1. 服务器密码与它的派生**（`src-tauri/src/core/pair/crypto.rs`、`server-relay/src/auth.rs`）
>
> ```text
> 服务器密码（任意文本，部署者在 .env 里设置）
>     --HKDF-SHA256(info="bongocat-pair-server-v1")--> SERVER_TOKEN   (32B -> base64url，43 字符)
>     SERVER_TOKEN --SHA256--> 中继保存的 verifier
> ```
>
> 与配对密码那三路输出（`auth` / `e2ee` / `room`）**彼此独立**，输入也完全不同：这里不是 32 字节密钥材料，而是部署者自己定的文本（客户端与中继都按 UTF-8 字节、先 `trim()` 再派生）。固定向量由两份 Rust crate 各写一遍（`bongo-server-password` → `qi0Bz36jIJ_OpjN86BJihPxefEIuxps8XLYTOBlmEmc`，另有一条中文密码守住「按 UTF-8 而不是 ASCII」）。中继与 Room 一样**只保存摘要**，启动之后进程里没有密码原文。
>
> **2. 线上只多一个头**：`X-Bongo-Server: <SERVER_TOKEN>`。`PROTOCOL_VERSION` / `FrameHeader` / `AppEnvelope` / `FrameKind` / AEAD 布局一行不改，所以：**新客户端**三边都能连（本版自建要它，旧自建与 CF 忽略未知头）；**旧客户端**连本版自建被 **HTTP 403** 挡下；**CF 版**一行没动（它没有服务器密码这回事，客户端不填那个框就不带这个头）。
>
> **3. 判定顺序把它放在最外层**：`路径 → 升级头 → WebSocket 版本 → X-Bongo-Protocol → 服务器密码(403) → ROOM_ID(400) → Authorization 非空(401) → 会话密钥/容量(401/503) → deviceId(400)`。放在 Room 之前是有意的：没有门槛的人不该能探测某个会话是否存在，更不该走到 `welcome` 那一步。
>
> **4. 用 403 而不是 401**：401 在这套协议里已经表示「配对密码不对」，而两者要给用户**不同的下一步**（401 → 找对方核对；403 → 找部署服务器的人要密码）。响应体也分开（`server password required` / `server password incorrect`），部署者一条 `curl` 就能自查。403 与 401 一样是 **fatal**：密码不对时重连一万次都会被同一句话挡回来。
>
> **5. 服务器密码必填**（`PAIR_SERVER_PASSWORD`，≥ 16 字符，启动时校验）：缺了就拒绝启动，而不是「默认开放」——一个默认开放的部署迟早被白嫖，而且用户不会知道。密码只发给要用的人；漏出去就换掉（所有人重填一次）。
>
> **6. 被 403 挡掉的尝试不占名额**：门槛在 `reserve` 之前，所以陌生人拿错密码刷多少次都不会把 `PAIR_MAX_SESSIONS` 占掉（集成用例 `a_rejected_server_password_never_consumes_a_session_slot` 用只有 1 个名额的服务器守着这条）。
>
> **7. 客户端只多一项本地凭据**：系统凭据库里的第二个条目（`server-password`），与配对密码互不覆盖——换服务器密码不该连带换掉 E2EE 密钥材料。`pair_connect` 现在接受两个可选值（`secret` / `server_password`），为空时才回落到凭据库；**界面层不再走这条「传值但不落盘」的捷径**（R37 第 1 条把它改成「连接即落盘」），IPC 上的这个能力只留给未来的调用方。
>
> **8. 设置页三项**：服务器地址 / 服务器密码 / 配对密码（用户可见文案同步改词为「配对密码」，不再出现 `Pair Secret`）。三项的控件**不是同一套**：地址不是秘密、随输随存，所以它没有「保存 / 已配置 / 删除」；两个密码各有「保存（清空输入框，只进系统凭据库）」与「已配置 / 删除」。三项的说明里都写出「由你们俩中的一个人部署服务器后一起提供」（只读审计的 P1-3：产品里原本没说地址从哪来）。
>
> **9. 部署侧同步落地的审计修复**：`PAIR_SERVER_PASSWORD` 进 `.env.example` 与两份 compose（`${PAIR_SERVER_PASSWORD:?…}`，缺了在 compose 阶段就明确报错）；`PAIR_DOMAIN` 示例值改回空（占位域名会让 Caddy 先启动成功、再在 ACME 里反复失败）；两份 compose 加 `healthcheck`（新子命令 `--health-check`：探一次本机 `/health`），Caddy 改成 `depends_on: service_healthy`；README 补齐**安全组放行 / 域名解析 / `curl /health` 自查**三步、1GB 机型的内存上限说明、以及「一帧约 400 字节（含 54 字节固定开销）」；`turnserver.conf` 修正 `external-ip`（coturn 只接受 IP 字面量）、补 `denied-peer-ip`（内网 / 回环 / 云元数据）与 `user-quota` / `total-quota` / `max-bps`；被拒绝的连接现在也会留一行不含秘密的日志。
>
> **10. 明确不改的一件事**：coturn 仍是静态凭据（`lt-cred-mech` + `user=`）。它现在只发给通过服务器密码的连接，所以够用；要更硬可以改成 `use-auth-secret` + 限时凭据，但那需要中继按会话签发 HMAC 凭据，属于下一步。
>
> **测试**：`server-relay` 新增单测（派生向量、verifier、会话层只认自己的密码）+ 集成（缺 / 错 / 对三种情况、两条 403 的响应体区分、403 不占名额、`/health` 的 `passwordRequired`），原有集成用例全部改为带 `X-Bongo-Server`（即「服务器密码对的时候，行为逐条不变」）；客户端新增派生固定向量、`X-Bongo-Server` 只在该带时才带、403 的文案与 fatal 分类；端到端新增 `BONGO_PAIR_E2E_SERVER_PASSWORD`（不设就不带头，旧中继与 CF 仍然跑得通同一批用例）。

> **R37（Phase 12 提交前的只读独立审计与修复）**
>
> 三个互不共享上下文的只读审计代理（协议与 Rust / 界面 / 部署），全程 **0 个 P0**：代码侧 `CLEAN`（6 条 P2），界面侧 1 条 P1 + 6 条 P2，部署侧 2 条 P1 + 13 条 P2。P0 / P1 全部修掉，P2 按「会不会真的绊到用户」取舍。
>
> **必修（P1）**
>
> 1. **界面：连接用的值与记住的值不是同一个**。`handleConnect` 把输入框里的值直接送给 Rust、但不落盘，于是出现自相矛盾的状态：界面显示「已连接」、输入框里明文还在，重启后凭据库却是空的（报「还没有配置配对密码」）；更坏的一种是框里残留着另一串**合法但不同**的配对密码——连接会成功，两个人却进了不同的会话，全程没有任何报错。现在改成**连接即落盘**：填了的字段先写进凭据库（指纹照常回显）、清空输入框，再连接，然后提示一句「已记住这次填写的值」。值只剩一个去处，界面状态、凭据库与线上连接不会再互相矛盾。
> 2. **部署：`.env` 里有些键根本传不到容器**。`PAIR_MAX_CHUNKS_PER_SECOND` / `PAIR_MAX_BYTES_PER_SECOND` / `PAIR_STALE_AFTER_MS` / `PAIR_ICE_SERVERS` 只写在 `.env.example` 与 README 表里，两份 compose 没有透传——照着改 `.env` 会「看着生效、其实没生效」，最典型的是 coturn 地址没进 `welcome`，双方都在 CGNAT 时打洞失败且毫无线索。两份 compose 现在全部按 `X: ${X:-}` 透传（留空即用二进制默认值），并写明「`.env` 是给 compose 做变量替换用的，容器不读它」。
> 3. **部署：coturn 的配额是按整台服务器算的**。`user-quota` 是「按用户名」计数的，而所有组共用同一张静态凭据（`iceServers` 是整台统一广告的，没法按组发不同凭据），所以原来写的 12 实际是**所有人合计 12 个分配**，约第 7 组（每组 ≈2 个分配）就可能拿不到 TURN；`min-port~max-port` 只留了 41 个端口，20 组也没有余量。现在按 `≈2 × (PAIR_MAX_SESSIONS + 2)` 改成 `user-quota=64` / `total-quota=128`、端口段放宽到 49160~49260，并在 conf 与 README 里点明「这几个数也是正常用量的天花板，不只是被白用的上限」。
>
> **顺手修掉的 P2（会真的绊到人的）**
>
> - **健康检查绑死回环**：`--health-check` 固定探 `127.0.0.1`，一旦 `PAIR_LISTEN` 写成具体网卡地址，容器就永远 `unhealthy`，而 Caddy 是 `depends_on: service_healthy`——整站起不来，症状只是「网站打不开」。现在探测地址跟着 `PAIR_LISTEN` 的主机走（通配才回落回环），两个超时收到各 1 秒（留出 compose `timeout: 3s` 的余量），并加了单测。
> - **403 的文案只管自建中继**：Cloudflare 边缘的 WAF 或企业代理也会用 403 拦 `/ws`，那时密码其实是对的。文案补成「若密码没错，多半是服务器前面的代理拦了连接」，仍然 fatal。
> - **凭据库读失败会挡住不需要服务器密码的连接**：读错误现在降级成「没有服务器密码」，官方 CF 中继照旧可连；真需要它的自建中继会用 403 说清楚该找谁要。
> - **删除凭据不可逆却点一下就删**：两个「删除」都补了确认（`Modal.confirm`，红色确认键）与一句「已删除」。
> - **连按回车会重入保存**：两个保存函数补 `saving` 守卫（按钮有 loading 挡着，键盘没有）。
> - **README 里的 `cargo run` 跑不起来**：仓库有 `bongocat-pair-relay` 与 `generate-pair` 两个可执行文件，裸 `cargo run` 会报「could not determine which binary to run」。README 两处都补了 `--bin bongocat-pair-relay`；顺手把 `--server` 的「生成 32 位」改成实际长度（24 个字符）、把 `Authorization: Bearer <PAIR_AUTH_TOKEN>` 改成「由配对密码派生，客户端自动带」（那个标识早已不是配置项）。
> - **被拒日志会被扫描器刷满**：404 那一路不再记日志（它跟凭据无关，客户端自己会看到「服务器地址路径不对」），其余拒绝路径照旧各留一行不含秘密的痕迹。
> - **两处断言不表达性质**：`assert_ne!(verifier[0], …)`（SHA256 没有这种性质，会以 1/256 的概率在无关改动后翻红）换成 verifier 的固定向量——那才是「中继只存摘要」这条纪律的守门人。
> - **coturn 配置的两处硬伤**：`no-tlsv1` / `no-tlsv1_1` 不在 coturn 4.18 的名字表里（会打 `Bad configuration format` 警告后忽略）、`no-cli` 已弃用；镜像自带的 `--external-ip=$(detect-external-ip)` 与 conf 里的 `external-ip` 同时给会打一条 ERROR，现在 command 显式指向配置文件（保留 `--log-file=stdout`，否则 `docker compose logs coturn` 什么都没有）。补了保留段与 TEST-NET 的 `denied-peer-ip`；README 写明「`turnserver.conf` 是被跟踪的文件，改完别提交」，并把 `realm` / `server-name` 的占位域名一起写进「记得改」清单。
> - **文档口径**：R36 第 8 条说「三项都有保存 / 已配置 / 删除」与实现不符（地址是随输随存，它不是秘密），已按实际控件改写。
> - **测试本身没证明它宣称的事**：`a_rejected_server_password_never_consumes_a_session_slot` 原来用同一个 Room 打三次错密码，把闸门误挪到 `reserve` 之后这条用例照样会绿。现在每次都换一个**全新**的 Room（服务器只有 1 个名额），闸门一旦后移就会拿到 503。
>
> **复审（同一批代理）又提了 5 + 7 条 P2，都已修**：
>
> - 界面：「连接即落盘」在**替换**掉旧配对密码时改说「原来的已被替换」（凭据库里的旧值读不回来，这件事不能不说）；「已记住」的提示挪到连接**之前**（连接失败时用户看到两个空框 + 一条红字，容易读成白填了）；`handleConnect` 补 `connecting` 守卫（连点两次会取消并重启同一条会话、连出两条提示）；zh 的地址与服务器密码说明里两句指代不清的话改直白；en-US 的 `pairSecret` 说明里 `secret` 混用改成 `pairing key`（与标签一致）。
> - 部署：`PAIR_LISTEN` 也改成 `${PAIR_LISTEN:-0.0.0.0:8080}` 透传（它是最后一个「写进 `.env` 却不生效」的键，健康检查已经能跟着它走了，这一改才是完整闭环），并补上「域名模式别改成 `127.0.0.1:8080`，否则 Caddy 连不上上游而健康检查照样 healthy」；`PAIR_ICE_SERVERS` 的写法在四处说明里统一成「写进 `.env`、不要加引号」，并注明 compose 里必须加引号；补「改完 `turnserver.conf` 要 `... restart coturn`」（只改挂载的文件时 `up -d` 不会重启容器——这是第二个会静默不生效的步骤）；coturn 的 external-ip 冲突注释改成「保留先读到的那份（配置文件里的）」；日志口径按**阶段**写清（六段记、还没进协议层的那几步不记）；README 两处「旧版」统一成「改造前的老版本」；healthcheck 的失败消息带上探测地址；coturn 的配额算式改成算得出 64 的写法，并注明按 4.18 核过。
>
> **第三轮（同一批代理，针对上面这批修复）又提了 3 + 4 + 6 条措辞级 P2，都已修**：替换旧配对密码时先比对指纹（同一串再粘一遍不再说「被替换」）；`valuesReplaced` 的文案改直白；`turnserver.conf` 的配额算式与 §7.3 的公式统一成能算出 64 的写法；`--log-file=stdout` 的作用改成准确的说法（coturn 默认本来就往 stdout 打）；`external-ip` 的理由改成「官方只支持 IP，写域名会被解析一次、解析到 CDN 边缘 IP 或解析失败都会坏」（结论不变）；`.env.example` 补齐 `PAIR_STALE_AFTER_MS`；`PAIR_LISTEN` 的提醒补上「改端口还要同时改 Caddy 上游或 direct 模式的 `ports`」；生成密码的字符集注释补上 `0` `1`；调大 `PAIR_MAX_SESSIONS` 时提醒端口段与安全组放行要同步放大；README 的日志那一节补「握手中断走另一条日志」。三个代理的最终结论都是 `CLEAN`。
>
> **提交前最后一次全跑的证据**：`server-relay` 单测 **41**（含 `main.rs` 的健康检查目标）+ 集成 **22**、`cargo clippy --all-targets` 无警告、`cargo fmt --check` 干净；`src-tauri --lib` **135 passed / 9 ignored**；真实中继端到端 **8/8**（含多 Room 与其中的 P2P），另外单独验过「不设 `BONGO_PAIR_E2E_SERVER_PASSWORD` 时两端都拿到 403 与那句中文指引」；`eslint` / `tsc --noEmit` / `vitest` 51 条全绿；zh-CN 与 en-US 的 pair 键集完全对称（各 129 条，占位符一致）。
>
> **未验证（不要当成已验证）**：Docker 构建与 `docker compose up -d` 实跑、Caddy 的 ACME、coturn 的真实启动与配额计数与打洞、1GB 机器上的 OOM 现象、真机双端 P2P 打洞。审计环境里没有 Docker 与 coturn，这几项只有静态核对与常量算术。
>
> **明确留着不做的界面问题**：Rust 侧用户可见的错误文案统一是中文，英文界面会看到中文（这是既有约定，与那 7 条文案一致）；`identity.displayName`（对方昵称）没有任何输入控件，两处显示永远走回退（Phase 1 起如此）；`crypto.rs` 的两条配对密码报错仍写着 `base64url` 与「应为 32 字节，实际 N 字节」（现在它在**保存阶段**就暴露，是唯一出口，改文案不动逻辑即可）；`zh-TW` / `vi-VN` / `pt-BR` 三套语言从来没有 pair 这组键（运行时回退到英文，见 R35 末尾）。

# 1. 目标与非目标

## 1.1 目标

- 普通用户只需要三个值（Phase 12）：**服务器地址** + **服务器密码**（部署者在服务器上设置，防止别人白用） + **配对密码**（两个人填一样的值就进入同一个双人会话）。
- 一套服务器同时承载 `PAIR_MAX_SESSIONS` 组互不可见的双人会话。
- 每个会话最多两台不同设备；第三台被明确拒绝。
- 会话、公告、信令、聊天、附件、桌宠状态、统计**一个字节都不能串房**。
- 既有能力零回归：WebRTC / DataChannel / 60Hz / reliable 通道 / 回落 / E2EE / 同 deviceId 顶替 / 陈旧顶替。
- 服务器仍然不知道：原始配对密码、E2EE 根密钥、消息明文。

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

| 状态码 | 含义                                         |
| ------ | -------------------------------------------- |
| 400    | `ROOM_ID` 或 `deviceId` 非法                 |
| 401    | 同一会话上摘要对不上，或没带 `Authorization` |
| 503    | 服务器会话已满（**只针对新会话**）           |
| 426    | 协议不支持 / 不是 WebSocket 升级             |
| 404    | 路径不对                                     |

`GET /health` → `{"ok":true,"protocol":1,"mode":"multi-pair"}`（鉴权之前返回，不含任何会话信息）。

关闭码沿用 v2：`4002` 顶替、`4003` 会话已满两台设备、`4004` 陈旧顶替、`1008` 协议/限流、`1009` 单帧过大、`1011` 内部错误。

# 4. 客户端改动

- `crypto::derive_room_id`（固定向量）。
- `SessionConfig.room_id`；`client::connect(relay_url, room_id, auth_token, device_id)`。
- `client::build_upgrade_url` / `is_plaintext_endpoint`（裸 IP 与本机地址 `localhost` → `ws://`）。
- `PairStatus.plaintext` + `PairStatus.relayUrl`（只用于界面提醒，并且只在提醒说的地址 == 输入框当前值时显示）。
- 命令 `pair_generate_secret`（CSPRNG 32 字节）。
- 设置页：服务器地址 / 配对密码 / 生成 / 复制 / 连接 + 明文提醒。

# 5. 测试与验收

必须满足：

1. 全新用户只知道「服务器地址 + 配对密码」也能联机（设置页文案与流程）。
2. 同一个密钥最多两台设备；第三台被明确拒绝（4003 → 「该联机会话已有两台设备在线」）。
3. 一套服务器同时跑 `PAIR_MAX_SESSIONS` 组互不可见的会话（负向断言：帧、公告都不串）。
4. 满员只挡新会话；已有会话不受影响；会话空了释放名额。
5. P2P / 60Hz / reliable / 回落行为零回归。
6. E2EE 承诺不变（服务器拿不到 secret / 根密钥 / 明文）。
7. CF 版继续工作（忽略新头，同一个 URL 继续可用）。
8. 服务器管理员不再配置任何密钥。

# 6. 实现记录（提交）

| Phase         | 内容                                                                                   | 提交                 |
| ------------- | -------------------------------------------------------------------------------------- | -------------------- |
| A             | `derive_room_id` + 固定向量                                                            | `9336db4`            |
| B             | 客户端 `X-Bongo-Room` + `SessionConfig.room_id`                                        | `9336db4`            |
| C/D/E         | `server-relay` 多 Room + 隔离 + 容量                                                   | `e97d783`            |
| F             | 设置页文案 + 生成配对密码 + 明文提醒                                                   | `2d095e9`            |
| G             | 部署简化（direct compose / .env.example / README）                                     | `e97d783`            |
| H（Phase 12） | `derive_server_token` + `X-Bongo-Server` + 中继 403 门槛 + 部署（含 R37 的部署侧修复） | `5856a75`            |
| I（Phase 12） | 客户端派生与 403 文案 + 设置页第三项（服务器密码） + 文档 R36 / R37                    | `747bb7f`、`ed429b5` |

（A/B 合成一个提交，C/D/E 与 G 合成一个提交，F 单独一个；Phase 12 分成「中继」「客户端」
「界面与文档」三个；以上提交都已推到 `origin/feat/pair-desktop-v1`。）

# 7. Phase 12：服务器密码（R36）

## 7.1 目标与非目标

- 目标：**知道地址不再等于能用服务器**。陌生人（没有服务器密码）连握手都拿不到；`server.welcome` 里的 TURN 凭据也只发给通过门槛的人；门槛之内的人仍然互不可见（Room 隔离一条不改）。
- 非目标：账号 / 每用户凭据 / 服务器端限速到人 / 动态签发 TURN 凭据 / 改 `PROTOCOL_VERSION`。服务器密码是**共享门槛**，不是身份系统。

## 7.2 线上契约（相对 §3 只增不改）

| 项        | 约定                                                                                                                     |
| --------- | ------------------------------------------------------------------------------------------------------------------------ |
| 请求头    | `X-Bongo-Server: <SERVER_TOKEN>`（新增；`SERVER_TOKEN = base64url(HKDF-SHA256(服务器密码, "bongocat-pair-server-v1"))`） |
| 判定顺序  | 服务器密码（403）排在 `ROOM_ID`(400) / `Authorization`(401) / 容量(503) 之前，也在 `reserve` 之前                        |
| 状态码    | `403 server password required`（没带）、`403 server password incorrect`（带错）；两者都与 `401` 一样是客户端侧 fatal     |
| `/health` | 多一个常量字段 `"passwordRequired":true`（不含任何会话信息）                                                             |
| 关闭码    | 不变                                                                                                                     |
| 帧格式    | 不变（`FrameHeader` / `AppEnvelope` / AEAD 布局一个字没动）                                                              |

## 7.3 部署契约

- `PAIR_SERVER_PASSWORD` **必填**、≥ 16 字符，启动时校验；中继只保存 `SHA256(SERVER_TOKEN)`。
- `.env.example` 留空值，两份 compose 用 `${PAIR_SERVER_PASSWORD:?…}` 让「忘了填」在 compose 阶段就报中文错。
- 生成入口：`cargo run --release --bin generate-pair -- --server`（`--all` 一次给服务器密码 + 配对密码）。
- 日志：**凭据与会话相关的**拒绝路径各留一行（对端地址 + 阶段 + 状态码；按阶段是协议版本 / 服务器密码 / `ROOM_ID` / `Authorization` / 容量 / `deviceId` 六段），不含密码 / token / 完整 `ROOM_ID`；「还没进到协议层」的那几步（404、缺 `Upgrade` / `Sec-WebSocket-Version` / `Sec-WebSocket-Key`）不记，避免被公网扫描器刷满（R37）。
- **`.env` 里的每一项都要在 compose 里逐项透传**（`X: ${X:-}`）：`.env` 是给 compose 做变量替换用的，容器不读这个文件本身——少一行就是「改了 `.env` 却完全没生效」（R37 的 P1-1）。留空值 = 用二进制里的默认值。
- coturn 的配额按**整台服务器**给（只有一张静态凭据、`iceServers` 是统一广告的）：每一组约占 2 个分配，所以 `user-quota ≈ 2 × PAIR_MAX_SESSIONS` 再留余量——默认 20 组给到 `user-quota=64` / `total-quota=128`（R37 的 P1-2）。`PAIR_MAX_SESSIONS` 调大时这两个数要跟着调，否则「双方都在大内网」的那几组会拿不到分配（日志正常、打洞失败）。
- 容器健康检查 `--health-check` 探的地址跟着 `PAIR_LISTEN` 的主机走（通配地址才回落回环），两个超时各 1 秒（compose 的 `timeout: 3s` 之内）。

## 7.4 验收

1. 不填服务器密码 → 403 `server password required`，客户端显示「服务器密码不正确或还没填：请向部署这台服务器的人索取」。
2. 填错 → 403 `server password incorrect`；同一个会话里配对密码还是对的也不放行。
3. 填对 → 与 Phase 11 逐条同行为（原有集成用例全部带上了这个头）。
4. 403 的尝试**不占** `PAIR_MAX_SESSIONS` 名额。
5. 两台设备只填对服务器密码、配对密码不同 → 各自建会话（互不可见），与 Phase 11 一致。
6. CF 版与旧自建中继不受影响（客户端不填那个框就不发这个头）。
7. **真中继上验过门槛真的在链路上**：设 `BONGO_PAIR_E2E_SERVER_PASSWORD` 时 8 条端到端用例全过；**不设**时同一批用例全部停在 `Error`，两端拿到的都是 403 与那句中文指引（不是「只在单测里对」）。
